# Munshi

Munshi is a local-first session archivist for AI coding harnesses. It captures completed sessions,
uses an installed coding-agent CLI to produce a durable summary, writes Markdown with structured
metadata, and can deliver the result to remote archives such as Notesmith.

The first release targets GitHub Copilot CLI on macOS and Linux. The architecture must remain open
to Claude Code, OpenAI Codex CLI, and other harnesses without coupling capture, summarization,
rendering, or delivery to one vendor.

## Status

This repository currently contains the implementation plan. No production code exists yet.

## Goals

- Capture Copilot CLI sessions without scraping terminal output.
- Produce one Markdown summary per logical session.
- Update the same summary when an old session is resumed.
- Use Copilot CLI in noninteractive mode as the initial summarization engine.
- Store summaries locally even when remote delivery fails.
- Deliver summaries to Notesmith without requiring Notesmith server changes.
- Keep session sources, summarizers, renderers, and destinations independently replaceable.
- Support macOS and Linux.
- Leave a clean path to full transcript backup and restoration in a later phase.
- Integrate naturally with Madari while remaining useful from any terminal.

## Non-goals for the first release

- Backing up complete transcripts, session databases, checkpoints, or rewind snapshots.
- Restoring sessions from a remote archive.
- Running an always-on daemon when hooks can provide lifecycle events.
- Supporting Windows.
- Calling model APIs directly.
- Providing a central archive server.
- Building a web dashboard.
- Supporting Claude Code or Codex in the initial implementation.

## Product decisions

| Area | Decision |
| --- | --- |
| Implementation | Rust workspace |
| Initial source | GitHub Copilot CLI |
| Summary timing | Once at session end |
| Resumed sessions | Update the existing report |
| Initial archive content | Summary and metadata only |
| Local output | Markdown with YAML front matter |
| Initial remote sink | Notesmith-native HTTP API |
| Future remote sink | Versioned generic webhook |
| Platforms | macOS and Linux |
| Operation | Hook-driven commands; no daemon initially |
| License target | Apache-2.0 or MIT; do not copy GPL Clerk code |

Rust is preferred because Madari already implements Rust-based agent-session discovery, filesystem
watching, SQLite indexing, and macOS/Linux support. Munshi must still remain a standalone program:
Madari integration should use a stable CLI or local protocol rather than making the GUI responsible
for capture and summarization.

## Prior art

### Clerk

[vulcanshen/clerk](https://github.com/vulcanshen/clerk) is the closest existing product. It turns
Claude Code sessions into local Markdown summaries using lifecycle hooks.

Ideas to retain:

- Hook-driven execution rather than polling.
- Cursor-based incremental transcript processing.
- Per-project configuration layered over global configuration.
- File locking for concurrent sessions.
- Explicit running, failed, retry, and kill states.
- Redacted operational logs.
- Aggregated reports built from existing summaries rather than raw transcripts.

Munshi should reimplement these patterns rather than copy Clerk code because Clerk is GPL-3.0.

### Aider

[Aider](https://github.com/Aider-AI/aider) implements recursive, token-budget-aware history
summarization. Munshi should use the same general strategy for sessions that do not fit in one model
context:

1. Divide the transcript at message boundaries.
2. Summarize older chunks.
3. Preserve the most recent turns directly.
4. Recursively summarize intermediate results when necessary.
5. Produce one final structured summary.

### Codex CLI

[OpenAI Codex CLI](https://github.com/openai/codex) provides useful storage patterns for the later
backup phase:

- Append-only JSONL as the durable source of truth.
- SQLite as a queryable index rather than the only copy.
- Background persistence.
- Explicit filtering of durable versus ephemeral events.
- Zstandard compression for cold session files.

### Copilot CLI native features

Copilot CLI already provides documented lifecycle hooks, noninteractive execution, session sharing,
and local session state. Munshi should prefer documented hooks and transcript references over
depending on private SQLite or session-state schemas.

## Architecture

```text
Copilot hooks
    |
    v
Session source adapter
    |
    v
Normalized session and event model
    |
    v
Redaction and transcript budgeting
    |
    v
Summarizer runner
    |
    v
Markdown renderer
    |
    +--------------------+
    |                    |
    v                    v
Filesystem sink     Notesmith sink
    |                    |
    +---------+----------+
              |
              v
       SQLite state store
```

The important separation is between the harness that produced the session and the tool used to
summarize it. A Claude Code session should eventually be summarizable by Copilot CLI, and a Copilot
session should not require Copilot to remain the summarizer forever.

### Workspace layout

```text
munshi/
├── Cargo.toml
├── crates/
│   ├── munshi-cli/
│   ├── munshi-core/
│   ├── munshi-source-copilot/
│   ├── munshi-summarizer-copilot/
│   ├── munshi-render-markdown/
│   ├── munshi-sink-filesystem/
│   ├── munshi-sink-notesmith/
│   └── munshi-state/
└── docs/
```

The initial implementation does not need every crate on day one. The boundaries should exist as
Rust traits or modules first; crates should be split only where they provide a real dependency or
reuse boundary.

### Core interfaces

```rust
trait SessionSource {
    fn ingest(&self, event: SourceEvent) -> Result<SessionUpdate>;
    fn load_delta(&self, session: &SessionRef, cursor: &Cursor) -> Result<TranscriptDelta>;
}

trait Summarizer {
    fn summarize(&self, request: SummaryRequest) -> Result<StructuredSummary>;
}

trait Renderer {
    fn render(&self, report: &Report) -> Result<RenderedArtifact>;
}

trait Sink {
    fn deliver(&self, artifact: &RenderedArtifact) -> Result<DeliveryReceipt>;
}
```

Agent-specific data belongs in an `extra` map or adapter-owned structure. Adding an agent should not
require changing every sink or renderer.

## Copilot session capture

Copilot CLI exposes lifecycle hooks. Munshi should install user-level hooks under the Copilot
configuration directory.

Use these events:

- `agentStop`: record the session ID, transcript path, working directory, and latest available
  source position.
- `sessionEnd`: finalize or revise the report.

`sessionEnd` does not itself provide the transcript path, so Munshi must persist the last path seen
for each session during `agentStop`.

The hook command must:

- Read one JSON payload from standard input.
- Validate the event schema.
- Acquire a short-lived per-session lock.
- Record source metadata transactionally.
- Queue or execute finalization.
- Return quickly enough not to degrade Copilot CLI shutdown.
- Never fail the originating Copilot session because archival failed.

The compatibility spike must verify that command hooks fire during:

- Normal interactive sessions.
- Noninteractive `copilot -p` sessions.
- Resumed sessions.
- Interrupted and force-closed sessions.

Private files such as Copilot's internal session SQLite database may be inspected by a future
best-effort adapter, but the MVP must not depend on their schema.

## Session identity and revisions

The stable report identity is:

```text
copilot:<session-id>
```

The identity does not include the end timestamp because resumed sessions must update the same
report.

State tracked per session:

```text
agent
session_id
project identity
transcript reference
source cursor
source content hash
summary revision
summary content hash
local Markdown path
Notesmith note path
Notesmith expected hash
last successful delivery
last error
```

When a session is resumed:

1. `agentStop` identifies new transcript material.
2. Munshi loads only the content beyond the stored cursor.
3. The previous summary and new transcript delta are sent to the summarizer.
4. The summarizer returns a complete revised summary, not an append-only fragment.
5. The local Markdown file is replaced atomically.
6. `summary_revision` is incremented.
7. Notesmith is updated using optimistic concurrency.

If cursor validation fails because the transcript was truncated or rewritten, Munshi falls back to
a complete re-read and records the reason.

## Summarization with Copilot CLI

Munshi should invoke the installed Copilot CLI noninteractively and provide the prompt through
standard input. The exact command must be confirmed during the compatibility spike, but the
intended shape is:

```bash
copilot -s --no-ask-user --model <configured-model>
```

No tool permissions should be granted. Summarization needs no shell, filesystem, URL, or MCP tools.
The transcript and previous summary are supplied directly.

The runner must provide:

- Configurable model selection.
- Hard execution timeout.
- Process-tree cancellation.
- Captured stdout and bounded stderr.
- Output-size limits.
- Clear classification of authentication, quota, timeout, malformed-output, and transient errors.
- Exponential retry only for transient failures.
- Redaction of configured environment-variable values from logs.
- No fallback that silently reports success with an empty or partial summary.

### Structured output

The prompt should request a stable JSON result that Munshi validates before rendering:

```json
{
  "title": "Implement session archival",
  "goal": "Build the first Copilot session capture path.",
  "work_completed": ["..."],
  "decisions": ["..."],
  "files_changed": ["..."],
  "commands_and_validation": ["..."],
  "open_items": ["..."],
  "tags": ["rust", "copilot-cli"]
}
```

If Copilot cannot reliably emit strict JSON, Munshi may delimit a JSON object inside text and reject
responses that do not validate. Markdown should never be accepted as the internal data model.

### Context budgeting

Munshi should estimate input size before launching Copilot.

For oversized input:

1. Preserve session metadata and the previous summary.
2. Split new transcript content on event or message boundaries.
3. Summarize chunks independently.
4. Merge chunk summaries into a final report.
5. Store the cursor only after the final report has been written successfully.

This makes retries safe and prevents losing transcript ranges after a partial failure.

### Cost controls

Copilot CLI programmatic execution is officially supported, but Copilot usage is metered through
GitHub AI Credits. Subscription access should not be treated as unlimited usage.

Munshi should include:

- Maximum summarizations per hour and per day.
- One final call per completed session by default.
- Delta-based revisions for resumed sessions.
- Configurable maximum transcript size.
- A dry-run mode that captures and renders fixtures without invoking Copilot.
- Status output showing calls, failures, and estimated input volume.
- Explicit opt-in per project.

## Privacy and redaction

The MVP archives summaries only. Raw transcripts remain at their existing local source and are not
sent to Notesmith.

Before invoking the summarizer, Munshi should support:

- Secret-pattern masking.
- Explicit environment-variable value masking.
- Optional home-directory and username masking.
- User-provided regular expressions.
- Excluding selected transcript event types.
- Excluding files or repositories by pattern.
- A project-level disable switch.

The transcript has already passed through the original Copilot session, but a second summarization
call is still a separate disclosure and cost event. Logs must never contain complete prompts or raw
transcripts by default.

## Markdown output

Example:

```markdown
---
schema_version: 1
id: copilot:abc123
agent: copilot-cli
session_id: abc123
project: munshi
repository: surdy/munshi
branch: main
started_at: 2026-07-11T17:00:00Z
updated_at: 2026-07-11T19:42:00Z
summary_revision: 2
source_cursor: 184
source_hash: sha256:...
tags:
  - rust
  - copilot-cli
  - session-archive
---

# Implement session archival

## Goal

Build the first Copilot session capture path.

## Work completed

- ...

## Decisions

- ...

## Files changed

- ...

## Commands and validation

- ...

## Open items

- ...
```

Do not embed absolute local paths in front matter by default. A configurable local-only metadata
mode may include them when desired.

Suggested local layout:

```text
~/.local/share/munshi/
├── summaries/
│   └── <project>/
│       └── <session-id>.md
├── state/
│   └── munshi.db
├── failed/
├── locks/
└── logs/
```

Platform-specific base directories should come from the standard operating-system directory APIs
rather than hardcoded paths.

## State and concurrency

SQLite should store session state, cursors, report revisions, and delivery attempts. Markdown
remains the durable human-readable output.

Requirements:

- One writer transaction per state transition.
- A per-session advisory lock around summarization.
- Atomic Markdown replacement using write, flush, and rename.
- A content hash for rendered output.
- Idempotent sink delivery.
- No cursor advancement until rendering and required local persistence succeed.
- Failed remote delivery must not roll back successful local persistence.
- Concurrent terminal sessions must not block unrelated session IDs.

Useful commands:

```text
munshi install
munshi uninstall
munshi status
munshi sessions
munshi summarize <session-id>
munshi retry <session-id>
munshi retry --all
munshi show <session-id>
munshi config check
munshi doctor
```

All query commands should support `--json` for Madari and other integrations.

## Configuration

Use TOML with global configuration plus an optional nearest-parent project override.

```toml
[capture]
enabled = true
source = "copilot"
summarize_on_session_end = true

[summarizer]
engine = "copilot-cli"
model = ""
timeout_seconds = 300
max_calls_per_hour = 10
max_calls_per_day = 50
max_input_bytes = 1000000

[output]
directory = "~/.local/share/munshi/summaries"
include_absolute_paths = false

[redaction]
mask_home_directory = true
mask_usernames = true
environment_variables = ["GITHUB_TOKEN", "GH_TOKEN", "COPILOT_GITHUB_TOKEN"]
patterns = []

[sinks.filesystem]
enabled = true

[sinks.notesmith]
enabled = false
base_url = "https://notesmith.example.com"
vault = "memory"
folder = "Agent Sessions"
token_env = "NOTESMITH_TOKEN"
max_attempts = 5

[project]
enabled = true
```

Configuration precedence:

1. Command-line flags.
2. Environment variables intended for secrets or deployment overrides.
3. Nearest project configuration.
4. User configuration.
5. Built-in defaults.

## Notesmith delivery

Notesmith already exposes a suitable JSON API:

```http
POST /api/v/{vault}/notes
Content-Type: application/json

{
  "title": "Implement session archival",
  "folder": "Agent Sessions/munshi",
  "content": "# Implement session archival\n...",
  "frontmatter": {
    "id": "copilot:abc123",
    "agent": "copilot-cli",
    "summary_revision": 1
  }
}
```

An existing note can be replaced through the Notesmith note update endpoint using its returned path
and `expected_hash`. Munshi should persist both values in SQLite.

Therefore the initial Notesmith sink requires no Notesmith server modification.

Delivery behavior:

- Create on the first successful summary.
- Update on resumed-session revisions.
- Treat a matching existing Munshi ID as the same logical report.
- Use optimistic concurrency rather than blind overwrite.
- Retry timeouts and server errors with exponential backoff and jitter.
- Do not retry validation or authentication errors indefinitely.
- Place exhausted deliveries in a dead-letter state.
- Keep local Markdown authoritative when Notesmith is unavailable.

## Generic webhook versus raw Markdown

Posting raw Markdown is simple, but it moves important metadata into ad hoc headers and offers no
natural place for revision, idempotency, attachment, or delivery information.

A generic webhook can use a versioned envelope:

```json
{
  "schema_version": 1,
  "event": "summary.updated",
  "id": "copilot:abc123",
  "idempotency_key": "sha256:...",
  "revision": 2,
  "metadata": {},
  "markdown": "# Implement session archival\n..."
}
```

Advantages:

- Versioned contract.
- Explicit create/update semantics.
- Idempotency and revision fields.
- Structured metadata without custom headers.
- Room for compressed transcript artifacts later.
- Clear success, duplicate, retry, and rejection behavior.

Disadvantage:

- The receiving server must implement the contract or use a translation proxy.

Notesmith does not currently accept this generic envelope or raw Markdown as its note-creation
request. The MVP should therefore implement a Notesmith-native sink behind the general `Sink`
interface. A generic webhook sink can be added later without changing capture or rendering.

## Madari integration

Munshi must work without Madari. When it is running inside Madari, the integration can be richer.

Madari already provides:

- A Rust backend.
- Agent-session discovery for Copilot, Codex, Claude, Gemini, and OpenCode.
- A derived SQLite session index.
- Filesystem watchers for agent stores.
- A session browser and resume flow.
- A local control socket.
- `MADARI_TAB_ID`, `MADARI_PANE_ID`, and `MADARI_CONTROL_SOCKET` in local panes.
- Status-pill and notification commands.
- Markdown rendering.

### Initial integration

A Munshi hook running inside Madari inherits the Madari context variables. It can report lifecycle
state without requiring an embedded plugin:

```bash
madari set-status --tab "$MADARI_TAB_ID" munshi "Summarizing"
madari notify "Session archived" "The Munshi summary is ready."
madari clear-status --tab "$MADARI_TAB_ID" munshi
```

Munshi should expose:

```bash
munshi sessions --json
munshi status --session <id> --json
munshi summarize <id> --json
munshi show <id> --json
```

Madari can then add:

- `summarizing`, `archived`, and `delivery failed` states to agent sessions.
- A "View summary" action in the agent-session browser.
- A "Summarize now" action.
- A rendered summary panel using its existing Markdown support.
- A link to the Notesmith note.
- A retry action for failed delivery.

### Integration boundary

Prefer a subprocess/JSON interface initially. This keeps Munshi independently releasable and avoids
making Madari responsible for hook installation, Copilot authentication, or background retries.

If repeated parsing code becomes a maintenance problem, extract only stable normalized types and
session-source parsers into a small reusable Rust crate. Do not share a writable SQLite database
between the applications.

An optional local socket protocol may replace repeated subprocess calls later. It should use the
same versioned request and response types as `--json`.

### Remote panes

Madari's local control socket is not available from remote SSH panes. Future options include:

- Run Munshi on the remote host and deliver summaries directly to Notesmith.
- Forward status through a dedicated OSC marker understood by Madari.
- Add a secure Madari bridge for remote agent events.

Remote integration is not part of the MVP.

## Future full-session backup

Full backup belongs in a later phase and uses a separate server repository. Munshi remains the
client responsible for discovering, packaging, uploading, verifying, downloading, and restoring
artifacts.

Potential artifact manifest:

```json
{
  "schema_version": 1,
  "agent": "copilot-cli",
  "session_id": "abc123",
  "captured_at": "2026-07-11T19:42:00Z",
  "files": [
    {
      "path": "events.jsonl.zst",
      "media_type": "application/x-ndjson",
      "compression": "zstd",
      "sha256": "..."
    },
    {
      "path": "session.db.zst",
      "media_type": "application/vnd.sqlite3",
      "compression": "zstd",
      "sha256": "..."
    }
  ]
}
```

Backup requirements:

- Snapshot SQLite consistently before compression.
- Hash every artifact.
- Encrypt sensitive archives in transit and preferably before upload.
- Resume interrupted uploads.
- Keep server and client schemas versioned.
- Verify checksums after download.
- Restore into a staging directory before replacing local state.
- Distinguish documented agent exports from private implementation files.
- Record the producing agent and version.

This phase should study Codex's append-only rollout and compression design but not assume Copilot,
Claude, and Codex share compatible session formats.

## Error handling and observability

Munshi should expose an explicit state machine:

```text
observed
capturing
ready
summarizing
rendered
delivering
complete
failed_summary
failed_delivery
```

Operational logs should include IDs, durations, byte counts, revisions, and error categories but
not raw transcript content.

`munshi doctor` should check:

- Copilot CLI exists and is authenticated.
- The configured model is usable.
- Hooks are installed and syntactically valid.
- State and output directories are writable.
- SQLite migrations are current.
- Notesmith is reachable when enabled.
- The target Notesmith vault exists.
- Required secret environment variables are present.

## Testing

### Unit tests

- Hook payload parsing.
- Session identity and revision behavior.
- Cursor validation and fallback.
- Redaction rules.
- Transcript chunking.
- Structured summary validation.
- Markdown golden files.
- Configuration precedence.
- Retry classification.
- Notesmith request construction.

### Adapter conformance tests

Each source adapter should consume recorded, sanitized fixtures and produce the same normalized
session model. Fixtures must include:

- New session.
- Multi-turn session.
- Resumed session.
- Truncated transcript.
- Missing optional fields.
- Interrupted session.
- Concurrent sessions in the same repository.

### Integration tests

- Fake Copilot executable with controlled stdout, stderr, timeout, and exit behavior.
- Local mock HTTP server for Notesmith create, update, conflict, timeout, and server-error paths.
- Real filesystem locks and atomic replacements.
- SQLite migration and crash-recovery tests.
- Kill the summarizer midway and verify no cursor advancement or partial Markdown.

### Live tests

An opt-in suite may invoke a real authenticated Copilot CLI. It must never run in normal CI or
consume credits without explicit enablement.

## Delivery phases

### Phase 0: Compatibility spike

- Verify Copilot hook behavior in interactive, noninteractive, resumed, and interrupted sessions.
- Capture sanitized hook and transcript fixtures.
- Confirm noninteractive prompt invocation and structured output.
- Measure transcript sizes and summary latency.
- Confirm how Copilot quota and authentication failures surface.

Exit criterion: one manually installed hook can identify a session and a standalone command can
produce a validated summary from its transcript.

### Phase 1: Local MVP

- Rust workspace and core data model.
- Hook installer and uninstaller.
- Copilot source adapter.
- SQLite state store.
- Copilot CLI summarizer.
- Context budgeting and redaction.
- Markdown renderer and filesystem sink.
- Resumed-session revision behavior.
- Status, retry, show, doctor, and JSON output.

Exit criterion: normal and resumed Copilot sessions reliably produce one atomically updated local
Markdown report.

### Phase 2: Notesmith

- Notesmith-native sink.
- Create and optimistic update.
- Retry and dead-letter state.
- Project folder routing.
- Link stored in local state and JSON output.

Exit criterion: Notesmith receives and updates the same logical report without losing the local
copy during outages.

### Phase 3: Madari

- Status-pill and notification integration.
- Summary status in the agent-session browser.
- View, summarize, and retry actions.
- Local Markdown rendering and Notesmith links.

Exit criterion: a user can discover, inspect, summarize, and resume a Copilot session from Madari
without Munshi becoming dependent on Madari.

### Phase 4: Additional harnesses

- Claude Code hook adapter.
- Codex rollout-file adapter and watcher.
- Adapter conformance suite.
- Optional alternative CLI summarizers.

Exit criterion: capture source and summarizer can be selected independently.

### Phase 5: Backup and restore

- Versioned artifact manifest.
- Zstandard packaging.
- Remote backup protocol.
- Resumable uploads.
- Download, verification, and staged restore.
- Separate archive server repository.

Exit criterion: a complete session can be restored and recognized by its originating harness after
round-trip backup.

## Risks

| Risk | Mitigation |
| --- | --- |
| Copilot hook or transcript format changes | Prefer documented payloads; version adapters; maintain fixtures and probes |
| Unexpected AI Credit usage | Finalize once, process deltas, impose limits, show usage |
| Summarizer hangs | Hard timeout and process-tree cancellation |
| Transcript contains secrets | Redact before summarization; never log prompts |
| Concurrent hook executions | Per-session locks and SQLite transactions |
| Notesmith unavailable | Local-first persistence, retry, dead-letter state |
| Resumed session creates duplicate note | Stable ID and revisioned update |
| Model emits malformed output | Validate structured output and fail explicitly |
| Tight Madari coupling | Standalone CLI and versioned JSON boundary |
| Private agent stores change | Keep them out of MVP contracts; defensive optional adapters |

## Open implementation questions

These should be resolved during Phase 0 rather than guessed:

- Exact Copilot transcript event schema exposed by `transcriptPath`.
- Whether hooks execute synchronously enough to require detached finalization.
- Best structured-output prompt and validation strategy for available Copilot models.
- Reliable mapping from transcript offsets to semantic event cursors.
- Whether the first release should keep state in one SQLite file or separate operational and
  delivery databases.
- Notesmith authentication as deployed behind the current reverse proxy.
- Naming and routing rules for Notesmith folders.
- Whether local summaries should be grouped by repository identity, filesystem project slug, or
  both.

## Reference links

- [Clerk](https://github.com/vulcanshen/clerk)
- [Aider](https://github.com/Aider-AI/aider)
- [OpenAI Codex CLI](https://github.com/openai/codex)
- [GitHub Copilot CLI documentation](https://docs.github.com/en/copilot/how-tos/use-copilot-agents/use-copilot-cli)
- [Running Copilot CLI programmatically](https://docs.github.com/en/copilot/how-tos/use-copilot-agents/use-copilot-cli)
- [GitHub Copilot hooks reference](https://docs.github.com/en/copilot/reference/hooks-reference)

