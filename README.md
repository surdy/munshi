# Munshi

Munshi is a local-first session archivist for AI coding harnesses. It captures completed sessions,
uses an installed coding-agent CLI to produce a durable summary, writes Markdown with structured
metadata, and can deliver the result to remote archives such as Notesmith.

The first release targets GitHub Copilot CLI on macOS and Linux. The architecture must remain open
to Claude Code, OpenAI Codex CLI, and other harnesses without coupling capture, summarization,
rendering, or delivery to one vendor.

## Status

The workspace includes the Phase 0 `munshi-probe`, standalone `munshi archive`, user-level hook
registration, SQLite-backed automatic archival with resumed revisions and interrupted-session
recovery, per-project policy with bounded hourly/daily/input/timeout/concurrency budgets that
defer rather than drop work, and optional dedicated archive Git history. Markdown remains the
durable archive. Operational status/query/retry contracts are now available through
`status`, `sessions`, `show`, `retry`, `retry-all`, `doctor`, and `configuration-check`.
Opt-in Notesmith delivery is now implemented (disabled by default) through the `munshi delivery`
commands: latest-revision create/replace with bounded retry and a dead-letter state, confirmed
backfill of existing summaries, and delivery state surfaced in `status`, `doctor`, and `show`.
The mandatory remote revision-history capability remains a later slice. See
[`docs/automatic-archive.md`](docs/automatic-archive.md) and
[ADR 0006](docs/adr/0006-deliver-to-notesmith-downstream-of-local-archival.md).


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
| Default consent | Registration enables local summarization for all projects; projects may opt out |
| Transcript protection | No secret redaction or granular event filtering in the first release |
| Summary history | Latest only by default; optional dedicated Git history |
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
Project policy and transcript budgeting
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

`munshi register` installs only `hooks/munshi.json` under `~/.copilot` or `$COPILOT_HOME`, after a
prominent disclosure and explicit acceptance. It writes direct `exec`/`args` entries for
`agentStop` and `sessionEnd` using the absolute current Munshi executable. Configuration and
rebuildable SQLite operational state are stored below the selected Munshi state directory;
unrelated hook and settings files are untouched. Restart Copilot CLI after registration because
hooks are loaded at startup. See
[automatic session archival](docs/automatic-archive.md).

Use these events:

- `agentStop`: record the session ID, transcript path, and origin working directory.
- `sessionEnd`: record completion and queue clean or interrupted archival.

`sessionEnd` does not itself provide the transcript path, so Munshi persists the last path seen for
each session during `agentStop`. This is the primary transcript lookup, but it is not sufficient:
an early Ctrl-C can produce `sessionEnd` without `agentStop`, and SIGKILL can produce neither hook.

Each hook command:

- Reads one JSON payload from standard input.
- Validates the event schema.
- Commits one short idempotent SQLite transaction while preserving the first origin directory.
- Reserves at most one detached worker for the session state generation.
- Starts clean-session archival in a detached process with inherited standard streams closed.
- Returns quickly enough not to degrade Copilot CLI shutdown.
- Never fails the originating Copilot session because archival failed.

A session becomes archive-worthy only after at least one user request and agent-produced content or
tool activity. Empty or cancelled starts may be recorded diagnostically but do not create Markdown.

Interrupted and force-close recovery follows these rules:

1. Use the latest hook-provided `transcriptPath`.
2. For a supported, version-pinned Copilot adapter, derive the expected transcript path from
   `sessionId`, require that it exists, and validate its expected envelope before use.
3. If neither lookup succeeds, leave the session pending rather than guessing or marking it
   archived.
4. Later hooks and the internal recovery command revisit retryable failures and stale known
   sessions after a quiet period.
5. A discovered session without a safely known origin is retained as unresolved operational state
   and is not summarized into a guessed project.

The recovered summary records the interrupted completion reason when known. A force-killed process
may provide no reason at all.

Source note: privacy-safe inspection of installed Copilot CLI 1.0.70 maps
`$COPILOT_HOME/session-state/<sessionId>/events.jsonl` deterministically. No documented CLI resolver
exists; an experimental RPC is not a production contract. The derived path is therefore a private,
version-pinned, existence-validated fallback, never a stable public contract. Normal capture must
continue to prefer the documented hook-provided `transcriptPath`.

Other private files such as Copilot's internal session SQLite database may be inspected by a future
best-effort adapter, but the MVP must not depend on their schema.

## Additional harness adapters

Munshi captures Claude Code and Codex sessions through the same vendor-neutral
[`SourceKind`](crates/munshi/src/source.rs) adapter boundary that Copilot uses. Selecting a source
is independent from selecting a summarizer: use `munshi archive --source claude-code|codex`, or
`StateStore::open_for_source` / `run_archive_worker_for_source` to drive the shared archive/state
worker pipeline for those harnesses. Claude Code normal/resumed/interrupted and Codex
normal/resumed transcripts normalize to the same model and archive through the shared summarizer,
renderer, state, and delivery paths. Existing Copilot behavior, identity (`copilot:<session-id>`),
and archive format are unchanged.

The version-pinned transcript schemas, normalized-record mapping, supported lifecycles, and
private-format assumptions are documented in
[harness source adapters](docs/harness-adapters.md). Synthetic/sanitized conformance fixtures live
under `fixtures/claude-code-2.1.44/` and `fixtures/codex-rollout-0.x/`.

## Session identity and revisions

The stable report identity is:

```text
<source>:<session-id>
```

where `<source>` is `copilot`, `claude-code`, or `codex`. Copilot identities remain `copilot:<id>`.
The identity does not include the end timestamp because resumed sessions must update the same
report. It also does not include a repository: one source session remains one logical session even
when it changes working directories or touches multiple repositories.

The first detected Git repository is the session's origin project and remains its routing
association. Project identity uses a normalized canonical remote when available so clones and
worktrees group together, with a locally assigned identity for repositories without a remote.

The SQLite store tracks current operational state per session:

```text
agent
session_id
first origin and project identity
latest transcript reference
record and byte cursor
prefix and full source hashes
lifecycle and completion reason
current structured-summary cache
current revision, Markdown hash, and relative path
processing attempts, leases, and safe failure category
```

For a resumed session:

1. `agentStop` identifies new transcript material.
2. Munshi validates the exact previously processed byte prefix.
3. Only records beyond the validated cursor are normalized.
4. The previous complete structured summary and new delta are sent to the summarizer.
5. The summarizer returns a complete revised summary, not an append-only fragment.
6. The local Markdown file is replaced atomically.
7. `summary_revision` is incremented.

If cursor validation fails because the transcript was truncated or rewritten, Munshi falls back to
a complete re-read, omits the previous-summary delta context, and records a content-free fallback
reason. Revision and cursor state advance only after the new Markdown has been durably replaced.

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
- No logging of transcript input or child-process environment.
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
- Project-level opt-out.
- Opportunistic retry of deferred work on later hooks and Munshi commands.

Implemented: global `--max-calls-per-hour`, `--max-calls-per-day`, and `--max-concurrency` at
registration; a nearest-parent `.munshi.toml` project override for `enabled`, `max_calls_per_hour`,
`max_calls_per_day`, `max_input_bytes`, and `timeout_ms`; `munshi project enable|disable|status`;
and deferral (not failure) of work that exceeds a budget or an explicit or override-based
disablement. See [`docs/automatic-archive.md`](docs/automatic-archive.md#project-policy-and-cost-budgets).
A dry-run mode and aggregated status output remain later slices.

## Privacy and disclosure

The MVP archives summaries only. Raw transcripts remain at their existing local source and are not
sent to Notesmith. They are sent as-is to Copilot CLI for summarization.

The first release intentionally does not implement secret redaction, user regexes, file exclusions,
or transcript-event filtering. Registration must prominently disclose that:

- summarization is enabled by default after registration;
- transcript content is processed by the configured Copilot model;
- projects can be disabled before or after registration;
- disabling a project stops future processing but does not delete existing archives.

The transcript has already passed through the original Copilot session, but a second summarization
call is still a separate disclosure and cost event. Logs must never contain complete prompts or raw
transcripts.

Generated summaries are structured work records, not compressed transcripts. They should capture
goals, decisions, meaningful changes, commands and validation, and open items without verbatim
prompts, raw tool output, secrets, or substantial code excerpts.

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

Archive files are Munshi-owned and may be atomically replaced. Human annotations belong in
separate notes.

Git history is optional. When disabled, Munshi retains only the current summary text. When enabled:

- the configured archive output directory is a dedicated Git repository;
- Munshi never commits summaries into source-code repositories;
- each successful summary revision produces one commit containing that archive-file change;
- the commit records the stable session ID and summary revision;
- local Git history may be enabled without a remote sink;
- if Notesmith delivery is enabled, Notesmith must preserve a separate correlated Git history or
  delivery is blocked.

## State and concurrency

`$COPILOT_HOME/munshi/munshi.db` stores session state, current cursors, worker claims, attempts, and
safe diagnostics. It uses forward migrations, foreign keys, WAL, short writer transactions, and a
brief migration advisory lock. Markdown, with optional future Git history, remains the durable
record; SQLite is rebuildable operational state rather than the authority for an existing archive.
See [ADR 0004](docs/adr/0004-use-rebuildable-sqlite-operational-state.md).

Implemented guarantees:

- One writer transaction per state transition.
- A persistent owner-only per-session advisory lock around source loading, summarization, Markdown
  persistence, and final state transition.
- Atomic Markdown replacement using write, flush, and rename.
- Current structured-summary and rendered-content hashes.
- No cursor advancement until rendering and required local persistence succeed.
- Post-rename/pre-transaction crash reconciliation from the planned hash and Markdown front matter.
- Backoff plus opportunistic retries for failed pending work.
- Safe import of stale issue #3 metadata/jobs and rebuild from current owned Markdown.
- Concurrent terminal sessions must not block unrelated session IDs.

`munshi hook recover` is an internal recovery/testing path used by later hooks and explicit repair
work.

## Operational CLI contracts

The following commands expose stable human-readable output and stable machine output with `--json`:

- `munshi status`
- `munshi sessions [--state <kebab-state>] [--limit N]`
- `munshi show <session-id>`
- `munshi retry <session-id> [--force]`
- `munshi retry-all [--limit N] [--force]`
- `munshi doctor`
- `munshi configuration-check`
- `munshi delivery status`
- `munshi delivery backfill [--confirm] [--limit N]`
- `munshi delivery retry [<session-id>] [--all] [--force] [--limit N]`
- `munshi delivery history [--configure]`

The delivery sink is managed with `munshi delivery configure --endpoint <url> --vault <name>
[--folder <path>] [--credential-env <VAR> | --credential-keychain <service:account>]
[--max-attempts N] [--provision-history]`, `munshi delivery enable`, and `munshi delivery disable`.
Delivery is disabled by default; the endpoint/vault/folder and the *source* of the credential are
recorded in the Munshi-owned `config.json`, but the credential itself is only ever resolved at
delivery time from an environment variable or the OS credential store. Enabling delivery reports
the pending backfill as a dry run so existing summaries require an explicit `munshi delivery
backfill --confirm` before publishing.

When local archive Git history is enabled (`--archive-git-history`) alongside delivery, delivery is
**versioned** (issue #9): each delivered revision is preserved as a correlated commit in the
Notesmith vault's own per-vault Git history, keyed by the same source-scoped session identity and
revision as the local archive commit. Munshi verifies the vault's revision-history capability
(`config.git.enabled`); if it is unavailable, versioned delivery is **blocked** with an actionable
status rather than degrading to latest-only storage. `munshi delivery history` reports the live
capability, `--configure` (or `--provision-history` at configure time) explicitly enables it, and
blocked deliveries recover via `munshi delivery retry` once the capability is present.

Every JSON response emits `schema_version: 1` and a command discriminator. Session states are
classified as `archived`, `revision-pending`, `summary-pending`, `interrupted`, `failed`,
`delivery-related`, `disabled-project`, `processing`, `observed`, `not-archive-worthy`, or
`unknown`.

`configuration-check` and `doctor` report:

- capture state (`enabled`, `disabled-project`, `unknown`) from real issue #5 policy state
  (`policy.disabled_projects`) rather than a local-archival approximation,
- delivery state (`disabled`, `enabled`, `delivery-related`, `unknown`), where `enabled` means
  delivery is on with an addressable sink and `delivery-related` means delivery is on but the sink
  is not yet addressable,
- archive Git-history configuration and repository health checks when enabled.

Retry commands are idempotent and reuse the same per-session lock, claim, and lifecycle transition
logic as hook workers. They do not bypass the SQLite state machine. By default they respect retry
backoff and permanent-failure markers; `--force` overrides those markers only for selected retry
targets. `show` returns only current summary and operational metadata, never raw transcript
content.

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
git_history = false

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

Implemented today: global configuration is written as `$COPILOT_HOME/munshi/config.json` from
`munshi register` flags (including `--max-calls-per-hour`, `--max-calls-per-day`, and
`--max-concurrency`) rather than a hand-edited global TOML file, and the nearest-parent project
override is the `.munshi.toml` file described in
[`docs/automatic-archive.md`](docs/automatic-archive.md#project-policy-and-cost-budgets). A
full user-editable global TOML file and environment-variable overrides remain later work.

## Notesmith delivery

Notesmith's create route assembles a note from a separate `frontmatter` map plus a body-only
`content` field, so Munshi sends the durable summary **body only** as `content` (the local archive's
own frontmatter is stripped) and supplies its Munshi-owned identity frontmatter separately:

```http
POST /api/v/{vault}/notes
Content-Type: application/json

{
  "title": "copilot-abc123",
  "folder": "Agent Sessions/munshi-<hash>",
  "content": "# Implement session archival\n...",
  "frontmatter": {
    "munshi_session": "abc123",
    "munshi_source": "copilot",
    "munshi_project": "github.com/owner/repo",
    "munshi_revision": 1
  }
}
```

An existing note is replaced through the Notesmith update endpoint using its returned path. Unlike
create, the update endpoint writes the request `content` verbatim, so Munshi sends a **complete,
single-frontmatter-block document** (identity frontmatter plus body) on replace — the
`munshi_session`/`munshi_source`/`munshi_project`/`munshi_revision` fields therefore stay present
and update on every revision. Munshi persists the returned note path, the delivered summary
revision, and the last summary hash in SQLite (schema-version 3 `deliveries` table). Because Munshi
owns delivered notes, the replace is sent without `expected_hash` so a later revision overwrites any
remote edits.

Notesmith itself is unauthenticated and defers authentication to a reverse proxy (notes-method
ADR 0010). When a credential source is configured, Munshi resolves a bearer token at delivery time
and sends `Authorization: Bearer <token>`; the token is never written to configuration, logged, or
echoed in any diagnostic.

Notes are routed by stable identity, not by the mutable summary title: a note is filed under
`<folder>/<origin-project-component>/` with the stable filename `<source>-<session-id>.md`, so the
persisted note identifier is idempotent across deliveries and even across an operational-database
rebuild (a create that conflicts is adopted as a replace).

Latest-revision delivery requires no Notesmith server modification. Versioned
(revision-history-preserving) delivery (issue #9) reuses the same wire protocol: the `NotesmithSink`
trait also verifies/configures the vault's per-vault Git capability and commits each delivered
revision, and delivered notes carry stable `munshi_session`/`munshi_revision` frontmatter that
correlates with the vault's Git history.

Delivery behavior (implemented):

- Create on the first successful summary, replace on later revisions.
- Treat a matching existing Munshi ID as the same logical report.
- Treat delivered notes as Munshi-owned and overwrite remote edits.
- When delivery is first enabled, report a count/dry run and require confirmation before
  backfilling existing current summaries.
- Retry transport and server errors with bounded exponential backoff.
- Place exhausted deliveries in a dead-letter state (`munshi delivery retry --force` revives them).
- A disabled project stops future delivery while retaining existing delivery history.
- Keep local Markdown authoritative and untouched when Notesmith is unavailable; a delivery outage
  never rolls back, invalidates, or blocks a local archive.

Versioned delivery (issue #9): when local archive Git history is enabled alongside delivery, Munshi
verifies (or, with `--provision-history`/`munshi delivery history --configure`, explicitly enables)
the Notesmith vault's revision-history capability. Each delivered revision is committed into the
vault's own Git history with a message correlated to the local archive commit by source-scoped
session identity and revision. If the capability is absent, versioned delivery is blocked with an
actionable `remote-history-unavailable` status and never degrades to latest-only storage; the block
never affects the already-successful local archive, and recovers on retry once the capability is
present.

Because Notesmith commits stage the whole vault working tree, Munshi runs a clean-tree preflight
before writing/committing: if the vault has unrelated uncommitted changes it blocks with
`remote-history-dirty` (the session's own note is allowed to be dirty) rather than bundling
unrelated work into the correlated commit. Delivery-then-commit is crash-safe: after a lost commit
response, an idempotent no-op, or a rebuilt operational database, Munshi recovers the existing
commit by an exact `git/log` message match, so one remote commit and its SHA survive a crash between
the commit and the local database write. Munshi does not claim exclusive one-file commits: a write
that races the narrow preflight-to-commit window can still be bundled and is surfaced as a
diagnostic.

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
- Transcript chunking.
- Structured summary validation.
- Markdown golden files.
- Configuration precedence.
- Retry classification.
- Notesmith request construction.
- Archive Git commit behavior.

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

The code-only probe foundation and safe usage instructions are documented in
[`docs/phase-0-probe.md`](docs/phase-0-probe.md). Live results belong in the explicitly unobserved
[`docs/phase-0-findings.md`](docs/phase-0-findings.md) matrix.

Exit criterion: one manually installed hook can identify a session and a standalone command can
produce a validated summary from its transcript.

### Phase 1: Local MVP

- Rust workspace and core data model.
- Hook installer and uninstaller.
- Copilot source adapter.
- SQLite state store.
- Copilot CLI summarizer.
- Context budgeting and explicit registration disclosure.
- Markdown renderer and filesystem sink.
- Project disablement.
- Optional dedicated archive Git history.
- Resumed-session revision behavior.
- Status, retry, show, doctor, and JSON output.

Exit criterion: normal and resumed Copilot sessions reliably produce one atomically updated local
Markdown report.

### Phase 2: Notesmith

- Notesmith-native sink.
- Create and Munshi-owned replacement.
- Confirmed backfill of existing current summaries.
- Versioned-delivery capability enforcement when local Git history is enabled.
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
| Transcript contains secrets | Prominent setup disclosure and project opt-out; redaction is deferred beyond v1 |
| Concurrent hook executions | Per-session locks and SQLite transactions |
| Notesmith unavailable | Local-first persistence, retry, dead-letter state |
| Resumed session creates duplicate note | Stable ID and revisioned update |
| Model emits malformed output | Validate structured output and fail explicitly |
| Tight Madari coupling | Standalone CLI and versioned JSON boundary |
| Private agent stores change | Keep them out of MVP contracts; defensive optional adapters |

## Open implementation questions

Remaining questions for later slices:

- Which observed Copilot transcript event variants are stable enough to normalize across versions.
- Reliability of the validated title/summary prompt across available Copilot models and larger
  inputs.
- Safe default quiet periods for force-close discovery across future Copilot versions.
- Notesmith authentication as deployed behind the current reverse proxy.
- How Notesmith exposes or verifies per-note Git revision history for versioned delivery.
- Naming and routing rules for Notesmith folders.

## Reference links

- [Clerk](https://github.com/vulcanshen/clerk)
- [Aider](https://github.com/Aider-AI/aider)
- [OpenAI Codex CLI](https://github.com/openai/codex)
- [GitHub Copilot CLI documentation](https://docs.github.com/en/copilot/how-tos/use-copilot-agents/use-copilot-cli)
- [Running Copilot CLI programmatically](https://docs.github.com/en/copilot/how-tos/use-copilot-agents/use-copilot-cli)
- [GitHub Copilot hooks reference](https://docs.github.com/en/copilot/reference/hooks-reference)
