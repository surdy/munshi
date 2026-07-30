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
Copilot row. Session-ID-only transcript discovery remains intentionally Copilot-only, because only
Copilot has a safe, version-pinned `session-state/<id>/events.jsonl` fallback; other sources are
left pending rather than guessed. Path-yielding directory sweeps are a different, permitted
mechanism: recovery also sweeps the registered Claude Code home's `projects/*/` directories for
stale regular `<session-id>.jsonl` files whose hooks never fired (force-kill emits none). Each
swept file provides its own explicit transcript path — no ID-to-path guessing occurs — and its
origin project is read from the transcript's pinned top-level `cwd` key. Sibling `<uuid>/`
directories, `memory/`, and foreign-envelope files are skipped by file-type, extension, and
envelope checks.

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
- Session-ID-only resolution is supported **only** for the Copilot version-pinned session-state
  directory. Claude Code and Codex require an explicit transcript path.
- Codex `originator`/`cli_version` and Claude `version` are recorded structurally in fixtures for
  provenance but are not required for normalization.
