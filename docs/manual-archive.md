# Manual Copilot session archive

`munshi archive` (alias: `munshi summarize`) is the standalone issue #2 path. It reads one
Copilot `events.jsonl`, sends a bounded normalized session to an explicitly selected compatible
summary executable on standard input, validates the complete structured result, and atomically
writes one Munshi-owned Markdown record.

Build it with:

```bash
cargo build -p munshi
```

Archive by explicit transcript path:

```bash
munshi archive 11111111-1111-4111-8111-111111111111 \
  --events /explicit/session/11111111-1111-4111-8111-111111111111/events.jsonl \
  --project-dir /explicit/project \
  --output-dir /explicit/munshi-summaries \
  --summarizer copilot \
  --summarizer-arg=-s \
  --summarizer-arg=--no-ask-user
```

Alternatively, omit `--events` and provide a session ID. Munshi then uses only the version-pinned,
existence-checked `$COPILOT_HOME/session-state/<session-id>/events.jsonl` fallback. An explicit path
must be a regular, non-symlink `events.jsonl` whose parent is the session ID. `session.db` is never
opened.

The summary executable is never implicit. Transcript and prompt data are supplied only on stdin.
Munshi owns a hard timeout, process-group cancellation, and stdout/stderr limits; errors report only
safe categories and byte counts. Tests use committed fake executables and consume no AI credits.

Syntactically malformed nonblank JSONL fails the archive rather than silently dropping source
material. Unknown event types and malformed known-event shapes are ignored and represented only by
an aggregate diagnostic count. A session needs a nonempty `user.message.data.content` plus nonempty
`assistant.message.data.content` or a valid tool execution event; otherwise the command writes
nothing and exits with status 2.

Output is:

```text
<output-dir>/<filesystem-safe-project-component>/<source-session-id>.md
```

Project identity is a normalized network Git remote when available. Repositories without one use a
stable hash of the canonical local repository root; the absolute path is not written to Markdown.
The manual record always has revision 1, a source-line cursor, and a SHA-256 source hash.

## Current schema assumptions

- Copilot 1.0.70 known records use `id`, ISO `timestamp`, `parentId`, `type`, and object-valued
  `data`, with optional `agentId` and `ephemeral`. Source line order is preserved.
- Only string-valued `user.message.data.content` and `assistant.message.data.content` become
  request/assistant text. Transformed content, reasoning, encrypted/opaque content, and system
  messages are excluded.
- Tool activity is `tool.execution_start` or `tool.execution_complete`, validated by required
  tool-call fields. Arguments stay opaque JSON; completion results may provide string
  `result.content`, processable textual `result.contents`, or both. Normalization also retains
  `error.message`, never base64 assets or resource links.
- Envelope timestamps are accepted only as RFC 3339 strings. The shutdown event's separate
  millisecond `sessionStartTime` is not treated as an envelope timestamp.

These aliases are intentionally defensive compatibility assumptions, not a public Copilot schema.
They must be reconciled with ongoing transcript-schema research before broad automatic ingestion.

## Automatic archival and resumed revisions

Issue #3 adds registration disclosure, idempotent user-hook installation/removal, fast fail-open
hook ingestion, and automatic clean-session finalization through this same archive path. See
[`automatic-archive.md`](automatic-archive.md).

Issue #4 replaces the hook file handoff with SQLite operational state, per-session advisory locks,
validated record/byte cursors, resumed delta summaries with the previous complete structured
summary, revision increments, rewrite/truncation fallback, retries, and interrupted/force-close
recovery.

The standalone manual command intentionally remains a full single-shot archive at revision 1. It
does not open the registered hook database or claim incremental resume semantics. Automatic
workers write archive front matter schema 2; the state rebuilder accepts both manual schema 1
records and automatic schema 2 records. A schema 1 record remains valid durable Markdown but lacks
prefix evidence, so its next automatic update performs one complete reread before establishing a
schema 2 cursor.
