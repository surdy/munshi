# Manual session archive

`munshi archive` (alias: `munshi summarize`) is the standalone issue #2 path, extended to every
supported harness. It reads one transcript for the selected source, sends a bounded normalized
session to an explicitly selected compatible summary executable on standard input, validates the
complete structured result, and atomically writes one Munshi-owned Markdown record.

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

`--source <copilot|claude-code|codex>` selects the capturing harness (default `copilot`;
source selection is independent of the summarizer — see
[`harness-adapters.md`](harness-adapters.md)). What `--events` must point at differs per
source: Copilot expects the session's `events.jsonl`; Claude Code expects the harness's
`<session-id>.jsonl` transcript; Codex expects the rollout file. In every case the path must
be a regular, non-symlink file, and for Copilot its parent directory must be the session ID
(for Claude Code and Codex the file stem is the session ID). `session.db` is never opened.

Alternatively — **for Copilot only** — omit `--events` and provide a session ID. Munshi then uses
only the version-pinned, existence-checked
`$COPILOT_HOME/session-state/<session-id>/events.jsonl` fallback. Claude Code and Codex have no
session-ID-only fallback on this command and require `--events`; for Codex there is no safe
session-ID lookup anywhere in Munshi (rollout files are not named after the session), so the
explicit path is the only way in.

Oversized event content is elided from the summarizer input and replaced with a claim-ticket
marker, using the same threshold as the hook pipeline: pass `--state-dir` so a registered
installation's configured `limits.max_event_text_bytes` applies; without it the 128 KiB default is
used. The manual path writes a standalone record and never uploads to Patwari, so claim tickets in
a manually produced summary are only redeemable once the same session is archived and uploaded
through the hook pipeline.

For Copilot sessions, the manual path also stages the allowlisted sidecar set (issue #23) into the
sibling `<session-id>.sidecar/` directory beside the Markdown — the same
`workspace.yaml`/`plan.md`/checkpoints narrative state the hook pipeline captures, with the same
bounds and exclusions; see
[the sidecar section of `harness-adapters.md`](harness-adapters.md#sidecar-artifacts-snapshot-artifact-set-v2-issue-23).
Staging is best-effort by contract: any capture problem skips the file and never fails the
archive. As with claim tickets, nothing is uploaded — the staged sidecars only reach Patwari when
the session goes through the hook pipeline.

The manual path never chunks: it always builds one one-shot (`phase: "complete"`) request, so a
session whose normalized request exceeds `--max-input-bytes` fails outright. The marathon
map-reduce (`chunk`/`reduce` phases) and the placeholder durability floor are hook-pipeline
features and do not apply here. The invocation bounds are the same flags with the same defaults
as registration: `--timeout-ms 300000`, `--max-source-bytes 67108864` (64 MiB raw transcript
read), `--max-input-bytes 8388608` (8 MiB normalized input cap), `--max-stdout-bytes 262144`,
`--max-stderr-bytes 65536`. `--max-input-bytes` must sit at or above the registered
`chunk_threshold_bytes` — the inverted relation is rejected before any work, for the reasons in
[`configuration.md`](configuration.md#the-size-knob-relation).

The summary executable is never implicit. Transcript and prompt data are supplied only on stdin.
Munshi owns a hard timeout, process-group cancellation, and stdout/stderr limits; errors report only
safe categories and byte counts. Tests use committed fake executables and consume no AI credits.
The repeatable `--summarizer-env KEY=VALUE` flag sets environment variables on the summarizer
invocation with the same semantics as the registered `summarizer.env` map (opaque to Munshi,
merged before Munshi's own `MUNSHI_SUMMARIZER_*` variables, reserved keys rejected); see
[`summarizers.md`](summarizers.md) for the wrapper contract that consumes them.

Syntactically malformed nonblank JSONL fails the archive rather than silently dropping source
material. Unknown record types and malformed known-record shapes are ignored and represented only
by an aggregate diagnostic count. A session must be archive-worthy — at least one user request
plus assistant content or a valid tool activity, in the source's own record shapes — otherwise the
command writes nothing and exits with status 2.

Output for Copilot keeps the original flat layout; every other source nests under a
source-prefix segment so same-ID sessions from different harnesses never collide (see
[`harness-adapters.md`](harness-adapters.md#shared-normalized-model)):

```text
<output-dir>/<filesystem-safe-project-component>/<source-session-id>.md
<output-dir>/<filesystem-safe-project-component>/<source-prefix>/<source-session-id>.md
```

Project identity is a normalized network Git remote when available. Repositories without one use a
stable hash of the canonical local repository root; the absolute path is not written to Markdown.
The manual record always has revision 1, a source-line cursor, and a SHA-256 source hash.

## Current schema assumptions

Transcript interpretation is no longer a per-command concern: the read-time interpreter in the
[`munshi-transcript`](../crates/munshi-transcript) crate (ADR 0011) types the full observed event
census for every source — content records, deliberately-ignored bookkeeping kinds, and an
`Unknown` fallthrough that is surfaced rather than silently dropped. The version-pinned envelope
and record mappings the manual path relies on for each of the three sources are documented once,
in [`harness-adapters.md`](harness-adapters.md), alongside the private-format risks each adapter
carries; this command adds no assumptions of its own. These mappings remain defensive,
version-pinned evidence for private harness formats, not public schemas — but they are the settled
interpreters the automatic pipeline runs on, not provisional research.

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
