# Automatic clean-session archival

Register with absolute paths and explicitly accept the disclosure:

```bash
munshi register --accept-transcript-processing \
  --summarizer /absolute/path/to/copilot \
  --summarizer-arg=-s \
  --summarizer-arg=--no-ask-user \
  --archive-git-history \
  --output-dir /absolute/path/to/munshi-summaries
```

Without `--accept-transcript-processing`, a terminal prompts for the exact text `I ACCEPT`;
noninteractive registration fails. `--dry-run` prints the managed paths without writing. The
disclosure states that transcript summarization becomes default-on for all projects, the full
transcript is sent again to the configured summarizer and may consume credits, v1 does not redact
secrets or provide granular filtering, output is local Markdown, and remote delivery remains
disabled.

Archive Git history is disabled by default. `--archive-git-history` enables one commit per
successful non-cursor summary revision in the configured output directory's dedicated repository.

The default locations are:

- hooks: `$COPILOT_HOME/hooks/munshi.json`, or `~/.copilot/hooks/munshi.json`
- state/config: `$COPILOT_HOME/munshi`, or `~/.copilot/munshi`

`--copilot-home` overrides the root. Registration writes only Munshi's dedicated hook and config
files, rejects symlinked or wrongly owned managed paths, and does not rewrite equivalent files.
`munshi unregister` removes only those two positively recognized files; archives and pending
diagnostic state remain.

Register, and unregister when an existing hooks directory contains managed removal work, serialize
changes with a persistent owner-only `hooks/.munshi-registration.lock` file and a nonblocking OS
advisory lock held for the complete operation. The file is intentionally not unlinked; an unheld
file is immediately reusable, and the kernel releases an active lock if the process exits or
crashes. Unregister does not create a missing hooks directory or lock; it may remove a positively
recognized config directly when no hooks directory exists.

The hooks use direct `exec`/`args` fields with a two-second timeout, read exactly one bounded JSON
object, and emit nothing. Each hook performs only a short SQLite transaction and detached-process
spawn:

- `agentStop` preserves the first origin working directory and refreshes the latest hook-provided
  transcript reference.
- `sessionEnd` records a safe completion category, queues clean or interrupted work, and reserves
  at most one worker for that state generation.
- Duplicate payloads are deduplicated by immutable observation keys and do not create duplicate
  revisions.
- Hook validation, state, spawn, and archival failures always return success to Copilot. Only
  content-free diagnostic categories are stored.

Tests can use the hidden `munshi hook wait` command to await terminal state deterministically.

That hook shape is intentionally guarded as a version-pinned Copilot CLI 1.0.70 compatibility
contract from Phase 0. Generic web documentation or a different locally installed Copilot version
is not treated as evidence that the schema changed.

## SQLite state, locking, and revisions

Operational state is `$COPILOT_HOME/munshi/munshi.db`. The synchronous `rusqlite` store uses
forward migrations, foreign keys, WAL, full synchronous commits, a short busy timeout, and brief
`BEGIN IMMEDIATE` transitions. It stores stable session identity, first origin, latest transcript
reference, lifecycle/completion state, current cursor and hashes, the current structured-summary
cache, current Markdown metadata, processing attempts, and safe error categories. It stores no raw
transcript and no historical summary bodies.

Each session has a persistent owner-only `locks/<session-id>.lock`. A detached worker holds its OS
advisory lock while it:

1. transactionally claims pending, interrupted, failed, or revision work;
2. validates the prior Markdown and exact processed transcript prefix;
3. normalizes only new records when the prefix matches;
4. asks the summarizer for a complete replacement summary using the previous complete summary and
   the delta;
5. records a metadata-only persistence plan;
6. atomically replaces Markdown; and
7. commits the new cursor, hashes, and revision.

SQLite transactions are never held while reading the transcript, running the summarizer, or
writing Markdown. Unrelated session lock files permit unrelated workers to run concurrently.
Kernel lock release plus processing-attempt plans recover stale claims and a crash after Markdown
rename but before the final database commit.

Archive front matter schema 2 records the dynamic revision, record/byte cursor, normalizer version,
prefix hash, full source hash, completion reason, and any cursor fallback reason. An unchanged
transcript is a no-op. A validated append sends only its delta. Truncation, rewrite, or an older
cursor format triggers a complete reread and fresh full summary; a failure leaves the previous
Markdown readable and does not advance its revision or cursor.

An unterminated invalid final JSONL record is classified as a retryable in-progress tail and never
advances the cursor. If a complete rewrite/truncation rereads as no longer archive-worthy, Munshi
preserves the previous archive and records `source-not-archive-worthy` rather than reporting a
successful revision or misclassifying the envelope.

## Optional archive Git history

When registration enables `--archive-git-history`:

- Munshi initializes or validates only the configured output directory as the archive repository.
- The archive repository must be dedicated; Munshi rejects commits when it resolves to the
  session's origin source project identity.
- Each successful non-cursor summary revision creates exactly one commit for exactly one archive
  file path.
- Commit messages include stable `session_id` and `summary_revision` correlation metadata.
- Commit failures leave SQLite archival state unchanged and clear partial staged changes before the
  attempt is marked failed.

## Interrupted and force-close recovery

`sessionEnd` has no transcript path. If no earlier `agentStop` supplied one, the Copilot CLI 1.0.70
adapter may derive `$COPILOT_HOME/session-state/<session-id>/events.jsonl`, but only through the
existing contained, canonical, regular-file resolver and expected-envelope validation. This is
private version-pinned evidence, not a public Copilot contract. Failure to resolve it safely leaves
work pending.

Later hooks start an opportunistic recovery sweep. Known sessions with no end event are considered
only after both their hook activity and transcript mtime pass a quiet period; the same per-session
lock and hash checks prevent races and duplicate revisions. The internal command below is available
for deterministic repair and tests:

```bash
munshi hook recover --state-dir /absolute/copilot-home/munshi \
  --stale-after-ms 1800000
```

`--force-retry` makes failed retryable work immediately eligible. `--rebuild-state` backs aside the
SQLite file, recreates the schema, and rebuilds current archive metadata and the current structured
summary cache from validated Munshi-owned Markdown. Existing Markdown is never deleted or made
invalid by a missing database.

Registration imports recognized stale issue #3 `sessions/latest.json` and pending-job state before
removing it. Fresh legacy workers are deferred, malformed or symlinked artifacts are left
untouched, and a legacy result alone never proves archival without corresponding valid Markdown.

## Project policy and cost budgets

Registration also stores a `policy` section in `$COPILOT_HOME/munshi/config.json`: global
`max_calls_per_hour`, `max_calls_per_day`, and `max_concurrency` defaults (`--max-calls-per-hour`,
`--max-calls-per-day`, and `--max-concurrency` at registration; 10/50/2 unless overridden), plus the
explicit `disabled_projects` list written by:

```bash
munshi project disable /absolute/path/to/project
munshi project enable /absolute/path/to/project
munshi project status /absolute/path/to/project
```

Project identity is the same normalized canonical remote (or local fallback) used for archive
routing, so disabling follows clones and worktrees. Disabling stops future processing and delivery
only; it never deletes local archives or delivered notes, matching the disclosed registration
behavior. Re-registering with the same or changed limits never silently re-enables a disabled
project: `register` preserves the existing `disabled_projects` list.

A project may also carry a nearest-parent override file, `.munshi.toml`, discovered by walking
upward from the session's origin directory (the same directory Copilot reported at `agentStop`),
analogous to how `.editorconfig` or `.git` are discovered:

```toml
[project]
enabled = false
max_calls_per_hour = 2
max_calls_per_day = 10
max_input_bytes = 500000
timeout_ms = 120000
```

Every field is optional; an absent field falls back to global configuration. Precedence is nearest
project override, then global configuration, then built-in defaults. A present but unparsable or
oversized/symlinked override fails closed: rather than silently continuing with default-on
processing, the project is treated as disabled (`project-override-invalid`) until the file is
fixed. The explicit `disabled_projects` list always wins over an override's `enabled = true`.

Hourly, daily, input-size, and timeout budgets, plus global worker concurrency, defer work instead
of discarding it:

- A disabled project (explicit or override) leaves its session in its current pending lifecycle
  state with `last_error_category` set to `project-disabled`, `project-override-disabled`, or
  `project-override-invalid`.
- Reaching `max_concurrency` live processing leases defers a not-yet-claimed session
  (`concurrency-deferred`) without claiming or penalizing it.
- Reaching a project's effective hourly or daily summarizer-call budget defers the session
  (`budget-hourly-exceeded` or `budget-daily-exceeded`) before invoking the summarizer, so the
  budget check itself never spends a Copilot call.
- `max_input_bytes` and `timeout_ms` remain enforced per call; an oversized or slow attempt is a
  retryable summary failure rather than a silent drop.

The concurrency check and the session claim happen inside one `BEGIN IMMEDIATE` SQLite
transaction (`StateStore::claim_session`), and the hourly/daily budget check and its reservation
happen inside another (`StateStore::reserve_summarizer_call`), invoked only once a real
summarizer call is about to be made. SQLite serializes writers across processes, so two workers
racing to claim a slot or a budget unit cannot both observe capacity and both proceed; the second
always re-reads a count that already reflects the first's committed decision. A crashed worker's
claim self-heals through its lease expiry (`count_active_processing` only counts live leases), so
concurrency capacity is never permanently lost to a dead process.

In every case the session is left retryable rather than failed permanently, so it is picked up
opportunistically by a later hook (`agentStop`/`sessionEnd` trigger a recovery sweep automatically)
or by `munshi hook recover`, once concurrency frees up, the budget window rolls over, or the project
is re-enabled. No diagnostic category or log ever contains transcript content. See
[ADR 0005](adr/0005-defer-project-policy-and-budgets-never-drop.md).

Opt-in Notesmith delivery is downstream of local archival and never blocks or rolls back an
archive; it is operated with the `munshi delivery` commands and disabled by default. See
[ADR 0006](adr/0006-deliver-to-notesmith-downstream-of-local-archival.md). Operational inspection
and repair use stable `status`, `sessions`, `show`, `retry`, `retry-all`, `doctor`,
`configuration-check`, and `delivery status`/`backfill`/`retry` commands (with `--json` machine
contracts).
