# Harness source adapters

Munshi separates the harness that produced a session (the **source adapter**) from the tool used
to summarize it. This document records the version-pinned assumptions, normalized-model mapping, and
supported lifecycles for the Copilot, Claude Code, and Codex adapters, and the private-format risks
that each one carries.

Transcript-record classification lives in the [`munshi-transcript`](../crates/munshi-transcript)
crate (ADR 0011): a streaming, lossless parser whose typed events, deliberately-ignored bookkeeping
kinds, and `Unknown` fallthrough are the authority this document describes. The `munshi` crate keeps
the operational adapter boundary — [`SourceKind`](../crates/munshi/src/source.rs), which bridges to
the crate's `Source` — and folds the event stream into `NormalizedSession`. Selecting a source
(`munshi archive --source <copilot|claude-code|codex>`, or `StateStore::open_for_source` /
`run_archive_worker_for_source` for the shared state pipeline) is independent from selecting a
summarizer: a Copilot summarizer can archive a Claude Code or Codex session and vice versa.

The three adapters are not the same support tier. Copilot and Claude Code are **fully
operational** sources: `munshi register --harness` accepts exactly these two, installs their
lifecycle hooks, records their homes in `SourceHomes`, and gives them recovery sweeps and
transcript-path derivation. **Codex is read-only**: it has no hook integration (a Codex hook
invocation fails with `unsupported-hook-source`), cannot be registered, has no home in
`SourceHomes`, and — because rollout files are not named after the session — no session-ID
path derivation and no origin recovery. A Codex session enters Munshi only through
[`munshi archive`](manual-archive.md) or an explicit transcript path.

## Shared normalized model

Every adapter maps its transcript to the same [`NormalizedSession`](../crates/munshi/src/source.rs):

- `user` events increment `user_requests`,
- `assistant` events increment `assistant_messages`,
- `tool` events increment `tool_activities`,
- unrecognized or metadata records increment `ignored_events`,
- `started_at` / `updated_at` come from the minimum/maximum record `timestamp`.

A session is archive-worthy once it has at least one user request and any agent-produced content or
tool activity. Identity is `"<source-prefix>:<session-id>"` and the archive frontmatter records the
`agent` label; both are derived from `SourceKind`, never hardcoded. The incremental cursor, byte
hashing, truncation detection, concurrency snapshotting, and revision machinery are source-neutral
and shared unchanged across all adapters.

Durable archive files are scoped by source so that two harnesses that share a project component and
session ID never collide. Copilot keeps its original `<component>/<session_id>.md` layout for
backward compatibility; every other source nests under a `<source-prefix>/` segment
(`<component>/claude-code/<session_id>.md`, `<component>/codex/<session_id>.md`). Archive scanning,
hydration, and rebuild dedup are keyed by `(SourceKind, session_id)` to match the SQLite
`UNIQUE(source_kind, source_session_id)` constraint, so same-ID sessions from different sources are
both retained and never cross-imported.

## Operational CLI contracts

The operational CLI is source-aware while preserving Copilot's existing behavior:

- `munshi sessions --json` list items include a `source` field (the source selector, e.g.
  `copilot`, `claude-code`, `codex`) and list sessions across all sources.
- `munshi show <id> --json` includes `session.source_kind`. The existing `session.source` field
  (transcript progress) is unchanged.
- `munshi retry <id>` and `munshi show <id>` accept an optional `--source <selector>`. When a
  session ID exists under more than one source and no selector is given, the command fails with an
  explicit ambiguity error instead of guessing; `retry --json` reports the resolved `source`, and
  `retry-all` items carry a `source`. The internal `hook-worker` accepts `--source` so recovery and
  retry route each session to its own adapter and source-scoped state.

These are additive fields on the `schema_version: 1` status contracts; Copilot output keeps
`copilot`/`copilot:<id>` identities and its flat archive layout.

## Recovery and archive Git history are source-scoped

Interrupted/stale recovery (`munshi hook recover` / `run_recovery`) reads work lists across all
sources but performs each per-session mutation against a store scoped to that session's own
`SourceKind`, and validates staleness with that session's adapter envelope. This prevents a
non-Copilot session from being skipped, mis-routed to the Copilot adapter, or given a duplicate
Copilot row. Session-ID-only discovery splits into two distinct mechanisms since issue #53. The
CLI resolution path (`resolve_session_reference` — what `munshi archive <session-id>` without
`--events` uses) remains intentionally Copilot-only, because only Copilot has a safe,
version-pinned `session-state/<id>/events.jsonl` fallback; other sources are left pending rather
than guessed. The **repair** path (`derive_transcript_path`) re-derives a lost or never-recorded
transcript path from a session ID for Copilot *and* Claude Code: Copilot through the same
version-pinned fallback, Claude Code by scanning the registered home's `projects/*/` directories
for a regular `<session-id>.jsonl` — the recovery sweep's own scan, with the same symlink and
envelope discipline, so the result is always an explicit in-home path, never a guess. Codex has
neither mechanism (its rollout files are not named after the session) and never derives.
Path-yielding directory sweeps are a different, permitted mechanism: recovery also sweeps the registered Claude Code home's `projects/*/` directories for
stale regular `<session-id>.jsonl` files whose hooks never fired (force-kill emits none). Each
swept file provides its own explicit transcript path — no ID-to-path guessing occurs — and its
origin project is read from the transcript's pinned top-level `cwd` key. Sibling `<uuid>/`
directories, `memory/`, and foreign-envelope files are skipped by file-type, extension, and
envelope checks. Envelope validation itself is read-disciplined: it scans at most 8 MiB of the
file looking for the first meaningful record (with the single line further clamped), so it is a
bound on how far Munshi will read to classify a candidate, never a statement about how large a
transcript may be. Every path the repair mechanism derives must pass the same pinned-envelope
check before it is trusted, so an unrelated file that merely occupies the expected location is
rejected.

When optional archive Git history is enabled, each commit subject carries the durable archive
identity `<source-prefix>:<session_id>` (matching the Markdown frontmatter `id`), and the commit
body records `id`/`source`/`session_id`/`summary_revision`. Because archive files are source-scoped
paths, same-ID cross-source revisions land on different files and produce distinct commits; the
crash-recovery dedup correlates by the source-qualified `id` line (with a legacy `session_id`
fallback for pre-source Copilot archives), so re-commits remain idempotent.

Adapter-specific records that carry no archive-worthy conversation content (Claude `summary`/
`system` bookkeeping, Codex `session_meta`/`turn_context`/`compacted`/`reasoning`) are treated as
ignored metadata. Model reasoning is deliberately dropped and never normalized into events.

## Copilot CLI (version-pinned to 1.0.70)

Unchanged from the original adapter. Transcript envelope: `{id, timestamp, parentId, type, data}`
JSONL under `$COPILOT_HOME/session-state/<sessionId>/events.jsonl`. Event types
`user.message`, `assistant.message`, `tool.execution_start`, `tool.execution_complete`. See
[phase-0 findings](phase-0-findings.md). The session-state path remains a private,
existence-validated fallback, not a public contract. Archived 1.0.5x snapshots additionally
carry a `session.usage_checkpoint` bookkeeping record (issue #34); it is typed ignored metadata
alongside the pinned lifecycle/bookkeeping events (`session.start`, `session.resume`,
`session.shutdown`, `session.model_change`, `assistant.turn_start`, `assistant.turn_end`,
`hook.start`, `hook.end`, `system.message`). The full-archive census (issue #45) surfaced nine
more historical bookkeeping kinds across pre-1.0.70 CLI versions — `permission.requested`,
`permission.completed`, `session.binary_asset`, `subagent.started`, `subagent.completed`,
`system.notification`, `session.permissions_changed`, `session.mode_changed`, and
`session.compaction_start` — all typed ignored metadata as well. A second census wave, surfaced
once the chunked-marathon giants uploaded (issue #45), added eight more: `abort`,
`session.compaction_complete`, `session.context_changed`, `session.error`, `session.info`,
`session.plan_changed`, `session.task_complete`, and `session.workspace_file_changed` — same
treatment.

### Sidecar artifacts (snapshot artifact set v2, issue #23)

Beside `events.jsonl`, the session-state directory holds harness sidecar files, an allowlisted
subset of which is captured into Patwari snapshots as `sidecar/<relative-path>` artifacts:
`workspace.yaml`, `plan.md`, `vscode.metadata.json`, `checkpoints/*.md`, and
`rewind-file-snapshots/{tracking,index}.json` — the small textual narrative state. Deliberately
excluded: `session.db` (a live SQLite, largely duplicative of the events), `rewind-file-snapshots/
backups/**` and `rewind-snapshots/**` (bulk content-addressed user-file blobs), `files/**`
(arbitrary workspace trees, including symlinked `node_modules`), and dotfiles. Capture is bounded
(64 files / 1 MiB per file / 4 MiB total), refuses symlinks, stable-reads each file, and is
staged into the archive output directory at archive time so upload retries re-serialize a
byte-identical manifest; sidecars are optional by contract and any capture problem skips the file
rather than failing the archive or upload. Claude Code and Codex contribute no sidecar artifacts:
Claude's `todos/` are ephemeral scratch state and its shell snapshots are timestamp-keyed
environment caches, neither worth archiving.

### Resume restore (issue #71) — not supported, and why

`munshi restore --resume` refuses a Copilot snapshot with a typed
`unsupported-harness` refusal rather than writing anything into a Copilot home. The record itself
restores normally; only the harness-home placement is refused.

The reason is honest ignorance, not effort. Copilot's resumable state is not confined to the
artifacts Munshi archives: the sidecar allowlist above deliberately excludes `session.db`, and
nothing is ever staged from the monolithic `session-store.db`. Whether `copilot --resume` can
reconstruct a session from a restored `session-state/<id>/events.jsonl` plus the allowlisted
sidecars, with no store rows, is **unknown** — and writing a guess into a harness home is the one
failure mode the [khata handoff](khata-handoff.md) warns about: *"never claim a snapshot is
restorable merely because upload completed."*

What a spike would have to establish, before any of this is implemented:

1. Restore one archived session by hand into a scratch `COPILOT_HOME` (events + allowlisted
   sidecars, no store rows) and record what `copilot --resume` does — lists it, refuses it, or
   silently starts an empty session.
2. If the store rows are load-bearing: what minimum row set makes a session discoverable, whether it
   is derivable from the archived artifacts alone, and whether writing into a live SQLite the harness
   owns is acceptable at all (Munshi has never written to a harness database, and doing so needs its
   own decision).
3. Whether the answer is version-pinned to 1.0.7x in the same way the transcript layout is.

Either outcome is publishable here: a supported flow, a degraded "the transcript is readable in
place" flow, or a recorded "not resumable without upstream support".

The census tail also surfaced four archive-observed **tool-activity** kinds that are content,
not bookkeeping (issue #51), all normalized as `tool` events: `skill.invoked` (the agent loaded
a skill — `name` required; `path`, `description`, `source`, `trigger`, `model`, and the full
SKILL.md `content` rendered when present, with oversized content elided downstream per
ADR 0010), `tool.user_requested` (a user-initiated tool call with the exact
`tool.execution_start` payload shape), and `external_tool.requested` /
`external_tool.completed` (MCP/external tool calls; `requested` carries the
`tool.execution_start` shape plus a `requestId`, `completed` carries only the correlating
`requestId` and is required to have one). Typing these was count-affecting —
`tool_activities` grows for sessions containing them — hence normalizer version 3.

## Claude Code (version-pinned to 2.1.44, re-validated structurally at 2.1.205)

**Source of truth.** Claude Code stores each session as JSONL at
`~/.claude/projects/<encoded-cwd>/<session-uuid>.jsonl`. The transcript file is named after the
session, so the adapter uses the file stem as the session identifier when resolving an explicit
path. These files are a private, undocumented store; the mapping is treated as version-pinned
evidence, not a stable contract. A 2.1.205 live probe
([phase-0 Claude Code findings](phase-0-claude-code-findings.md)) confirmed the `user`/`assistant`
envelope is unchanged and that the record types added since 2.1.44 (`ai-title`, `attachment`,
`last-prompt`, `mode`, `queue-operation`) degrade to ignored metadata. Live-archive verification
(issue #30) surfaced four newer session-bookkeeping kinds the pinned schema predates —
`permission-mode`, `pr-link`, `file-history-delta` (sibling of `file-history-snapshot`), and
`frame-link` — all typed ignored metadata as well, and the full-archive census (issue #46) added
`agent-name` to the same class.

**Envelope.** Each line is a JSON object with a string `type` and (for turns) a `message` object,
plus bookkeeping keys such as `uuid`, `parentUuid`, `sessionId`, `timestamp`, `cwd`, `version`,
`gitBranch`, `isSidechain`, and `userType`.

**Record mapping.**

| Record | Normalized as |
| --- | --- |
| `type: "user"`, `message.content` string or `text` blocks | `user` event |
| `type: "user"`, `message.content` `tool_result` blocks | `tool` event (`event=tool_result`) |
| `type: "assistant"`, `text` blocks | `assistant` event |
| `type: "assistant"`, `tool_use` blocks | `tool` event (`event=tool_use`) |
| `type: "summary"` (compaction), `type: "system"`, queue bookkeeping | ignored metadata |
| `type: "permission-mode"` / `"pr-link"` / `"file-history-delta"` / `"frame-link"` / `"agent-name"` (archive-observed session bookkeeping) | ignored metadata |

**Lifecycles.**

- **Normal** — user prompt, assistant replies, and paired `tool_use`/`tool_result` blocks.
- **Resumed** — the transcript opens with a `summary` compaction record and continues appending to
  the same session; Munshi reprocesses it as a delta into a new summary revision on the stable
  archive path.
- **Interrupted** — the transcript ends without a clean end and may contain a `system` interruption
  notice (ignored metadata). Munshi records the interrupted completion reason supplied by the
  recovery/caller path; the transcript shape itself is not used to infer interruption.

Claude Code does not emit an explicit clean-end marker inside the JSONL, so completion reasons come
from the caller, exactly as they do for Copilot's `sessionEnd` hook. No unsupported lifecycle is
claimed.

### Resume restore (issue #71) — supported

`munshi restore --session <id> --resume --yes` places an archived Claude Code session back into a
harness home, because this harness's resumable state is very nearly the transcript itself: the
archived `transcript.jsonl` is written verbatim to
`<claude home>/projects/<cwd-slug>/<session-id>.jsonl`, and `claude --resume <session-id>` continues
the conversation.

**The slug.** The `projects/` subdirectory name encodes the session's working directory: every
character that is not an ASCII letter or digit becomes `-`, **one `-` per UTF-16 code unit**. So
`/Users/x/repos/munshi` → `-Users-x-repos-munshi`; a space, `.`, `_` and `+` each yield one `-`; `ü`
(one UTF-16 unit) yields one and an emoji (a surrogate pair) yields two. Restore recomputes it from
the `cwd` the archived transcript's own records carry, because on a wiped machine nothing else knows
it.

The rule was established empirically against Claude Code 2.1.227 on macOS, and is version-pinned
evidence like the rest of this adapter: (1) all 29 directories of a real `~/.claude/projects` matched
it when compared against the `cwd` their transcripts record — including a path containing spaces,
which rules out a narrower separators-only rule; (2) the harness was run in directories named
`ünï_x+y` and `a🎉b` under a throwaway `CLAUDE_CONFIG_DIR`, and created `--n--x-y` and `a--b`, which
is where the per-code-unit detail comes from. If the harness changes its encoding, restore places a
transcript the harness will not find; the reported target path is what makes that visible.

**Guarantees and their limits.**

- A transcript already at the target path is replaced **never**: byte-identical is a no-op, and
  differing is a refusal that `--force` does not lift (it is a live session the harness continued
  past the snapshot).
- The session's working directory may not exist on the new machine. That is a reported warning, not
  a refusal — the session still lists and resumes; tools that expect the directory will not find it.
- Sidecars are expected to be absent: Munshi deliberately archives no Claude Code sidecars (#23), and
  `todos/` and shell snapshots are ephemeral by design.
- Version compatibility is **reported, not enforced**. The archived version comes from the `version`
  key the writing harness stamps on turn records (snapshot manifests carry an optional
  `capture.source_agent_version` that Munshi does not populate today), and the installed version from
  a best-effort `claude --version`. Either being absent, or the two differing, is a stated warning.
  Restore never claims a snapshot is resumable because it uploaded.
- The post-check verifies that the transcript is readable at the derived path. It does **not** launch
  the harness to confirm discovery, so "the harness listed it" is not asserted by any automated test;
  that remains a manual acceptance step.

## Codex CLI (version-pinned to the rollout schema in openai/codex)

**Source of truth.** Codex CLI appends rollout files at
`~/.codex/sessions/YYYY/MM/DD/rollout-<timestamp>-<uuid>.jsonl`. Each line is a `RolloutLine`
(`{timestamp, type, payload}`) wrapping a tagged `RolloutItem`. The stable session identity is the
`session_meta.id`; when resolving an explicit path Munshi uses the file stem. This is a
private/rapidly-evolving format: the pinned reference is the `RolloutItem`/`ResponseItem` definition
in `openai/codex` (`codex-rs/protocol/src/protocol.rs` and `models.rs`).

**Record mapping.**

| Rollout item | Normalized as |
| --- | --- |
| `response_item` → `message` role `user` (`input_text`) | `user` event |
| `response_item` → `message` role `assistant` (`output_text`) | `assistant` event |
| `response_item` → `function_call` / `custom_tool_call` | `tool` event |
| `response_item` → `function_call_output` / `custom_tool_call_output` | `tool` event (string or `content_items` output) |
| `response_item` → `local_shell_call` | `tool` event |
| `response_item` → `reasoning` | ignored (internal model output) |
| `session_meta`, `turn_context`, `compacted`, `event_msg` | ignored metadata (typed) |
| any other top-level or `response_item` kind | `Unknown` — reported by `verify-archive-parse` as an interpretation gap, never silently ignored |

**Lifecycles.**

- **Normal** — `session_meta`, `turn_context`, then user/assistant messages and function calls.
- **Resumed** — `codex resume` writes `session_meta` with `parent_thread_id`/`forked_from_id` and a
  `compacted` record, then continues; Munshi ignores the metadata and archives the continued
  conversation.

Codex does not persist an explicit interruption record in the rollout, so an interrupted Codex
lifecycle is not claimed as a distinct supported scenario; the normal/resumed rollouts flow through
the shared pipeline and any completion reason is supplied by the caller.

### Resume restore (issue #71) — not supported

Refused with the same typed `unsupported-harness` refusal as Copilot. Codex contributes no sidecar
artifacts, has no `SourceHomes` entry to place into, and its rollout files are not named after the
session, so even the target path would be a guess. Nothing here has been probed; it stays out of
scope until someone needs it.

## Fixtures and conformance

Synthetic/sanitized conformance fixtures live under `fixtures/claude-code-2.1.44/` and
`fixtures/codex-rollout-0.x/`. They are hand-authored from the public schemas above — **no private
transcript content is copied** — and cover missing fields, truncated (incomplete trailing record)
transcripts, concurrent sessions, and source-specific metadata. The archive-observed bookkeeping
kinds (issues #30/#34 and the full-archive census #45/#46) are exercised by hand-authored sessions
under `fixtures/claude-code-2.1.2xx-bookkeeping/` and `fixtures/copilot-1.0.5x-bookkeeping/`. `crates/munshi/tests/harness_adapters.rs`
exercises normalization, foreign-envelope rejection, the one-shot archive pipeline, and the shared
archive/state worker pipeline (resumed revisions, interrupted completion reason, source isolation).

## Assumptions and open risks

- Claude Code and Codex stores are **private and undocumented**; both mappings are version-pinned
  evidence for the observed/published schema, not stable contracts. Adapters degrade to ignored
  metadata for unknown record and block types rather than failing.
- CLI session-ID-only resolution is supported **only** for the Copilot version-pinned
  session-state directory; Claude Code and Codex require an explicit transcript path on
  `munshi archive`. The internal repair path (`derive_transcript_path`, issue #53) additionally
  re-derives Claude Code paths by scanning the registered home's `projects/*/<session-id>.jsonl`
  — see [Recovery](#recovery-and-archive-git-history-are-source-scoped) above. Codex has no safe
  session-ID lookup of any kind.
- Codex is manual-archive/explicit-path only: no hooks (`unsupported-hook-source`), not
  accepted by `--harness`, no `SourceHomes` entry, no path derivation, no origin recovery.
- Codex `originator`/`cli_version` and Claude `version` are recorded structurally in fixtures for
  provenance but are not required for normalization. Claude `version` is additionally *read* by
  resume restore (issue #71) as the only harness-version evidence an archived session carries — as
  something to report, never as a gate.
- Writing into a harness home happens in exactly two places, both deliberate and both
  Claude Code only: hook installation at `munshi register`, and resume restore's single
  `projects/<slug>/<session-id>.jsonl` write under `--resume --yes`. Everything else — capture,
  sidecar staging, memory-sync collection — is strictly read-only.
- Resume restore's post-check proves the transcript is readable where the harness looks, not that
  the harness accepted it. "Machine A's session resumes on machine B" is a manual acceptance step;
  no automated test can drive a real harness.
