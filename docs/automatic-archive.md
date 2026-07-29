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

With no `--harness` (or explicit home) flag, registration targets every harness whose home
directory exists. `--harness copilot` and `--harness claude-code` (repeatable) select explicitly;
passing `--copilot-home` or `--claude-home` selects exactly that harness. Both harnesses share the
same state store, summarizer configuration, and archive tree.

Without `--accept-transcript-processing`, a terminal prompts for the exact text `I ACCEPT`;
noninteractive registration fails. `--dry-run` prints the managed paths without writing. The
disclosure states that transcript summarization becomes default-on for all projects, the full
transcript is sent again to the configured summarizer and may consume credits, v1 does not redact
secrets or provide granular filtering, output is local Markdown, and remote delivery remains
disabled.

Archive Git history is disabled by default. `--archive-git-history` enables one commit per
successful non-cursor summary revision in the configured output directory's dedicated repository.

The default locations are:

- Copilot hooks: `$COPILOT_HOME/hooks/munshi.json`, or `~/.copilot/hooks/munshi.json`
- Claude Code hooks: managed entries inside `$CLAUDE_CONFIG_DIR/settings.json`, or
  `~/.claude/settings.json`
- state/config: `$MUNSHI_HOME`, or `~/.munshi` (harness-neutral; ADR 0008)

`--copilot-home` overrides the Copilot hooks root, `--claude-home` the Claude Code home, and
`--state-dir` the Munshi home. Registration writes only Munshi's dedicated hook and config files,
rejects symlinked or wrongly owned managed paths, and does not rewrite equivalent files. Claude
Code's `settings.json` is a foreign file Munshi merges into: registration appends one strictly
shaped matcher group per managed event (`Stop`, `SessionEnd`) and preserves every other key,
event, entry, the user's key order, and the file's mode; a file that is not a JSON settings
object is refused before configuration is written. `munshi unregister` removes only positively
recognized Munshi-owned files and managed entries; archives and pending diagnostic state remain.
Register with no active Claude Code session: Claude Code itself rewrites `settings.json` and hook
configuration is read at session startup.

Register and unregister serialize changes with a persistent owner-only
`<state>/locks/.munshi-registration.lock` file and a nonblocking OS advisory lock held for the
complete operation. The file is intentionally not unlinked; an unheld file is immediately reusable,
and the kernel releases an active lock if the process exits or crashes. Unregister does not create
a missing hooks directory; it removes the hook installations the stored configuration records and
may remove a positively recognized config directly when no hooks directory exists.

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

## Claude Code hooks

Claude Code's `Stop` event (once per completed assistant turn) maps to the agent-stop ingestion
path and `SessionEnd` to session-end, via `munshi hook <event> --source claude-code` shell
commands with the same two-second timeout. The payload contract is version-pinned at 2.1.205
([phase-0 Claude Code findings](phase-0-claude-code-findings.md)): snake_case fields, no
timestamp (receipt time is stamped locally), `transcript_path` present on both events (persisted
as the transcript reference, so Claude Code never needs session-ID-only resolution), and extra
undocumented fields that tolerant deserialization ignores — hook payloads carry conversation
text and are never logged or persisted. `SessionEnd` reasons `clear`, `logout`, and
`prompt_input_exit` record an affirmative completion; every other value — including `other`,
which clean noninteractive runs also report — degrades to the unknown completion category and
still archives. Failures fail open toward Claude Code exactly as they do toward Copilot.

Because a force-killed Claude Code process emits no hooks at all, recovery additionally sweeps
the registered Claude home's `projects/*/` directories for stale, unknown `<session-id>.jsonl`
transcripts and archives them as interrupted (see
[harness adapters](harness-adapters.md)).

## SQLite state, locking, and revisions

Operational state is `$MUNSHI_HOME/munshi.db` (default `~/.munshi/munshi.db`). The synchronous `rusqlite` store uses
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

A transcript larger than the configured `max_source_bytes` fails as `source-failed` and is parked
rather than retried, since the same limit would reject it again. That verdict is tied to the
configuration that produced it: `retry`, `retry-all`, and the recovery sweep re-measure parked
transcripts against the currently configured limit and make sessions that now fit eligible again,
so raising `max_source_bytes` is enough — no `--force` needed (issue #44).

A retryable failure schedules the session's next attempt with an escalating per-session backoff
derived from its consecutive-failure streak — 10 minutes after the first failure, then 30 minutes,
90 minutes, 4 hours, capped at 24 hours — and any successful attempt resets the streak. After 5
consecutive failures with the same category against the same session content, the failure is
treated as deterministic and the session is parked (like the `source-failed` park above, with the
real category retained), so one broken session can no longer win the `max_concurrency` slots every
sweep and starve the rest of the queue (issue #38). Plain sweeps and `retry-all` never touch such a
park; a targeted `munshi retry <id>` lifts it explicitly, and `--force` (or `--force-retry` below)
lifts it for bulk retries — every lift, including the automatic `source-failed` re-measurement,
restarts the streak so the session gets fresh attempts. Sweeps also scan eligible work
least-recently-attempted first, and `munshi status` reports currently parked sessions as
`parked=<n>` on its sessions line.

A session whose origin directory was deleted after the fact no longer parks permanently as
`project-failed` or `origin-unresolved`. When the directory cannot be resolved on disk, the worker
derives the project identity from the origin evidence the source records already carry — Claude
Code's per-record `cwd` and `gitBranch`, Copilot's `workspace.yaml` `cwd` — using the same
remote-less stable-hash rule, and the recovery sweep hydrates parked `origin-unresolved` rows the
same way (issue #40). Archive frontmatter flags such records with `project_origin: "recorded"`.
Quiet-period gating is unchanged, and a transcript with no recorded origin evidence still parks
as before.

Two failure classes get a durability floor instead of staying unarchived forever (issue #43): a
summarizer process rejection (nonzero exit, typically oversized input beyond the model's real
capacity) that reaches the park threshold above, and Munshi's own `max_input_bytes` cap, which is
deterministic on the first attempt and carries its own `summary-input-limit` category. In both
cases the session archives anyway with a machine-generated placeholder summary: the Markdown is
flagged `summary_placeholder: true` in its frontmatter, tagged `munshi-placeholder-summary`, and
its body states plainly that the summary is unavailable and why. That unlocks the normal
downstream pipeline — the verbatim transcript uploads to Patwari and the placeholder note is
delivered to Notesmith, both self-describing — so the durable archive never silently misses
exactly the biggest sessions. The session itself stays `failed` and parked, recording that a real
summary is still owed: `munshi status` counts these as `placeholder=<n>`, and a targeted
`munshi retry <id>` (or new session activity) re-attempts a real summary, which replaces the
placeholder as the next revision through the ordinary revision machinery and re-uploads. A
placeholder never overwrites an existing real summary: once a session has a real revision, a
failed re-summary keeps that revision and the ordinary backoff/park verdict applies.

Marathon sessions no longer need the floor at all in the common case (issue #48): when the
measured size of a session's normalized summarizer request exceeds `limits.chunk_threshold_bytes`
(default 6 MiB, calibrated on real rejection data), the worker summarizes it in chunks instead of
one shot. The normalized event stream is split on record boundaries into segments of roughly
`limits.chunk_size_bytes` (default 2 MiB), the summarizer is invoked once per segment with a
`phase: "chunk"` request (carrying the previous segment's summary for continuity), and once with a
`phase: "reduce"` request that synthesizes the segment summaries into the one archived session
summary — recursing through intermediate reduces in the rare case the reduce input itself exceeds
the threshold (see [`summarizers.md`](summarizers.md) for the contract). Every invocation is
separately bounded by `timeout_ms` and separately charged against the per-project budget, and a
failure mid-chunk abandons the whole attempt into the ordinary backoff above — partial summaries
are never archived. The floor still catches what chunking genuinely cannot fix: a request no
split can bring under the threshold (for example one enormous un-elided event, or an irreducible
reduce input) floors immediately under `summary-input-limit`, and repeated chunk-phase summarizer
rejections floor at the park threshold exactly like one-shot rejections. Below the threshold,
`max_input_bytes` governs the one-shot request exactly as before.

`--force-retry` makes failed retryable work immediately eligible. `--rebuild-state` backs aside the
SQLite file, recreates the schema, and rebuilds current archive metadata and the current structured
summary cache from validated Munshi-owned Markdown. Existing Markdown is never deleted or made
invalid by a missing database.

Registration imports recognized stale issue #3 `sessions/latest.json` and pending-job state before
removing it. Fresh legacy workers are deferred, malformed or symlinked artifacts are left
untouched, and a legacy result alone never proves archival without corresponding valid Markdown.

## Project policy and cost budgets

Registration also stores a `policy` section in `$MUNSHI_HOME/config.json`: global
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

Opt-in Notesmith summary delivery is downstream of local archival and never blocks or rolls back
an archive; it is operated with the `munshi summary-delivery` commands (`delivery` remains a
deprecated alias) and disabled by default. See
[ADR 0006](adr/0006-deliver-to-notesmith-downstream-of-local-archival.md). Opt-in archive upload
to Patwari runs in the same position — downstream of local archival, in parallel with delivery,
never blocking either — and uploads each successful summary revision as a full snapshot (verbatim
transcript, rendered summary, extracted outputs). It is operated with the `munshi archive-upload`
commands, is disabled by default, and failed uploads are retried with backoff by later hooks and
`munshi hook recover`; see [ADR 0009](adr/0009-archive-full-snapshots-to-patwari.md) and the
[user guide](user-guide.md). Operational inspection
and repair use stable `status`, `sessions`, `show`, `retry`, `retry-all`, `doctor`,
`configuration-check`, `summary-delivery status`/`backfill`/`retry`, and `archive-upload
status`/`retry` commands (with `--json` machine contracts).
