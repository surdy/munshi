# Automatic clean-session archival

Register with absolute paths and explicitly accept the disclosure:

```bash
munshi register --accept-transcript-processing \
  --summarizer /absolute/path/to/copilot \
  --summarizer-arg=-s \
  --summarizer-arg=--no-ask-user \
  --output-dir /absolute/path/to/munshi-summaries
```

Without `--accept-transcript-processing`, a terminal prompts for the exact text `I ACCEPT`;
noninteractive registration fails. `--dry-run` prints the managed paths without writing. The
disclosure states that transcript summarization becomes default-on for all projects, the full
transcript is sent again to the configured summarizer and may consume credits, v1 does not redact
secrets or provide granular filtering, output is local Markdown, and remote delivery remains
disabled.

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

Remote delivery, project policy/budgets, optional Git history, and broad status/retry commands
remain out of scope.
