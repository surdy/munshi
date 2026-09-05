# Troubleshooting

Munshi's hooks are deliberately **fail-open**: a broken hook, a full disk, a misconfigured
summarizer, or a crashed worker must never break your coding-agent CLI. That means failures are
silent by design — nothing pops up mid-session to tell you a session didn't archive. This is the
playbook for "my session didn't archive, why?"

Every failure Munshi records is a short, content-free **category string** (never transcript
content). This doc maps those categories, and the symptoms that lead to them, to fixes.

## Start here, for any problem

```bash
munshi doctor                        # registration, config, and hook health
munshi status                        # rollups, plus the last recorded failure
munshi sessions --state failed       # sessions whose last attempt errored
munshi sessions --state interrupted  # sessions ended without a clean signal
munshi show <session-id>             # one session's full state and summary
```

`munshi status` prints the most recent failure as a single line:

```text
last failure: operation=<op> code=<category> session=<id-or-empty>
```

`operation` is which hook/worker step failed (`agent-stop`, `session-end`, `archive-worker`,
`recovery`, ...); `code` is the category explained below. Add `--json` to `status`, `sessions`,
or `show` for the exact machine-readable fields instead of parsing text.

`munshi doctor` (and the lighter, session-state-free `munshi configuration-check`) run a battery
of named checks, each `ok`, `warning`, or `error`. The ones worth knowing:

| Check | Failing means |
| --- | --- |
| `config-file` / `config-version` | `config.json` missing, unreadable, or the wrong schema version (current: 2; a version-1 file loads fine and auto-migrates on the next configuration load — see [`configuration.md`](configuration.md)). Otherwise re-run `munshi register`. |
| `state-directory-match` | `config.json`'s recorded state directory doesn't match the `--state-dir`/`$MUNSHI_HOME` this command is using — usually a copied config or wrong `--state-dir`. |
| `transcript-disclosure` | Registration never completed acceptance of the disclosure. Re-register with `--accept-transcript-processing`. |
| `summarizer-path` / `output-path` | The configured summarizer executable or output directory isn't an absolute path. Re-register with absolute paths. |
| `hook-file` / `hook-contract` / `hook-parse` / `hook-read` | Copilot's `hooks/munshi.json` is missing, unparsable, unreadable, or doesn't match the managed contract. |
| `claude-hook-contract` | Claude Code's `settings.json` is missing Munshi's managed `Stop`/`SessionEnd` entries, has stale entries (pointing at an old binary path), or isn't a JSON settings object Munshi recognizes. |
| `runtime-compatible` | Roll-up: `warning` here means *something* above isn't right even if each individual check looks close; automatic hook-driven archiving may not work until it clears. |
| `size-cap-parked` | Sessions are permanently parked on a size cap, split by which one: `source-oversized` (raise `--max-source-bytes`) or `summary-input-limit` (raise `--chunk-threshold-bytes`). Re-register with a larger limit, then `munshi retry-all --force`. |
| `phantom-invocations-parked` | Parked rows that are phantom CLI invocations, not sessions — no transcript was ever written (see ["My session never appeared"](#my-session-never-appeared)). `munshi retry-all --force` settles them `not-archive-worthy` (issue #58). |
| `transcript-missing-parked` | Sessions parked because their transcript no longer exists at its recorded path. Restore the file(s), or accept the loss with `munshi settle-lost --all-missing` (issue #58). |
| `capture-failing` | Five or more source-read failures (`source-missing`/`source-oversized`/`source-failed`) in the last 24 hours — transcripts are vanishing or unreadable *while* sessions are being captured, which deserves attention now, not a line item found later (issues #57/#58). |
| `input-cap-relation` | A hand-edited `config.json` holds `max_input_bytes` below `chunk_threshold_bytes` — the inverted relation `register`/`archive` reject at the flag. Sessions between the two values floor to placeholder summaries under `summary-input-limit` instead of chunking. Re-register with `--max-input-bytes` at or above `--chunk-threshold-bytes`, then `munshi retry-all --force`. |
| `summarizer-exhaust-home` | The configured summarizer-exhaust retention home overlaps a registered source home; retention is refused and nothing is ever pruned (issue #60). Point it at a genuinely isolated home. |
| `summarizer-exhaust-size` | The isolated summarizer home has grown past the size threshold — check that `munshi tick` is actually scheduled to prune it (issue #60). |

See [`docs/user-guide.md`](user-guide.md) for the day-to-day meaning of `munshi sessions --state`
values and the retry/recovery commands referenced throughout this doc.

## "My session never appeared"

Work through these in order:

1. **Hooks only load at CLI startup.** Both Copilot CLI and Claude Code read their hook
   configuration once, when a session starts. If you registered (or re-registered) while a
   session was already running, that session was never covered — restart Copilot, or start a
   *new* Claude Code session, after registering.
2. **Trivial sessions are correctly skipped.** A session only archives if it has at least one
   user request *and* at least one assistant message or tool activity. A session that never got
   a real reply is filed as `not-archive-worthy` in `munshi sessions` — this is expected
   behavior, not a bug, and there is nothing to retry. A session refused under the
   `summarizer-exhaust` diagnostic is a different matter — see
   ["Sessions I never started"](#sessions-i-never-started-with-a-summarizer-exhaust-diagnostic).
3. **Force-killed sessions arrive later, not never.** `SIGKILL` (or any hard crash) during an
   active turn emits no hook at all — there's no event to fail open from. Every hook invocation
   opportunistically kicks off a background recovery sweep that, after a quiet period, finds
   stale sessions with no end event (and, for Claude Code, also scans
   `~/.claude/projects/*/` for orphaned `<session-id>.jsonl` transcripts) and archives them with
   an `interrupted` completion reason. Give it a few minutes; if you don't want to wait, run
   `munshi retry <id>` or `munshi retry-all` directly, or force the sweep now with
   `munshi hook recover` (hidden from `--help`; a recovery command — use your actual state
   directory, `~/.munshi` unless you set `$MUNSHI_HOME`):
   ```bash
   munshi hook recover --state-dir ~/.munshi --stale-after-ms 1800000
   ```
   `munshi tick` is the scheduled/manual maintenance entry point that runs this same sweep (plus
   the park lifts and retention passes) — if you have it on a timer, the sweep is already
   happening every interval; running `munshi tick` by hand works too.
4. **A `source-missing` park with nothing behind it is a phantom invocation, not a lost
   session.** A parked row at revision 0 with no previous read and no transcript on disk is a
   non-interactive `claude` subcommand that fired the `SessionEnd` hook without ever writing a
   transcript (issue #58) — there was never a session to archive. The worker settles new ones as
   `not-archive-worthy` on its own; rows parked before that behavior existed drain through
   `munshi retry-all --force`, which settles them the same way. `munshi doctor` counts them
   under `phantom-invocations-parked`.
5. **Check `munshi doctor`'s hook checks.** `claude-hook-contract` (or `hook-contract` for
   Copilot) failing usually means the hooks aren't installed the way Munshi expects — see
   Registration problems below.
6. **A moved or rebuilt binary breaks the hook path.** Registration bakes the *absolute path* of
   the `munshi` binary you ran `register` with into the hook entries. If you rebuild or relocate
   that binary, existing hooks still point at the old path and silently no-op. Re-run
   `munshi register` after moving/rebuilding the binary.

## "Session failed with `summary-failed`"

`summary-failed` means your configured summarizer executable was invoked but its output (or
exit) didn't satisfy Munshi's contract. See [`docs/summarizers.md`](summarizers.md) for the full
contract; the common causes are:

- The process printed something other than exactly one JSON object matching the required
  eight-field schema (a Markdown fence, leading commentary, an extra field, an empty
  `work_completed` list — the one list that must have at least one item; the other five may be
  empty).
- The process exited nonzero.
- The process ran past `--timeout-ms` (default 300000ms) and was killed.
- stdout exceeded `--max-stdout-bytes` (default 262144) or stderr exceeded `--max-stderr-bytes`
  (default 65536).
- No split could bring a request under `--chunk-threshold-bytes` (default 2621440) — for example
  one enormous un-elided event. This produces a summary failure before your summarizer is even
  invoked. (`--max-input-bytes` sits at or above the threshold and is only a never-exceed
  backstop; see [`configuration.md`](configuration.md#the-size-knob-relation).)

Test the summarizer standalone before blaming Munshi, using the sample request committed at
`fixtures/summarizer/sample-request.json` (from a checkout of this repository):

```bash
cat fixtures/summarizer/sample-request.json | /absolute/path/to/your-summarizer | python3 -m json.tool; echo "exit=$?"
```

If that doesn't cleanly print all eight fields, fix the summarizer (or its wrapper — see
`contrib/claude-summarizer.sh` for a reference fix that strips Markdown fences and backfills
empty lists; only `work_completed` strictly needs the backfill, but blanket backfilling is
harmless) before retrying Munshi. Once it's fixed:

```bash
munshi retry <session-id> --source <copilot|claude-code> --force
```

`--force` is required here because a `failed` session normally waits out an escalating retry
backoff before becoming eligible again — 10 minutes after the first failure, then 30 minutes,
90 minutes, and 4 hours; `--force` skips that wait and restarts the escalation. After 5
consecutive failures with the same category on unchanged session content, Munshi stops retrying
entirely and parks the session so a deterministically failing summarizer cannot occupy the
`--max-concurrency` slots every sweep (issue #38). Parked sessions show up as `parked=<n>` on
the `munshi status`/`doctor` sessions line (and `munshi show <id>` prints the failure streak and
park); recovery sweeps skip them until you either fix the cause and run a targeted
`munshi retry <session-id>` (which lifts the park even without `--force`) or new activity
arrives in the session.

Two deterministic cases additionally trigger the placeholder durability floor (issue #43) instead
of leaving the session unarchived forever: a summarizer rejection (nonzero exit) that reaches the
5-failure park, and a genuinely unchunkable request, which fails immediately under its own
`summary-input-limit` category. Both archive the session with a machine-generated placeholder
summary (frontmatter `summary_placeholder: true`, tag `munshi-placeholder-summary`) so the
transcript still uploads and delivers; the session stays parked and is counted as
`placeholder=<n>` on the `munshi status` sessions line. Fix the summarizer (or raise the size
limits) and run a plain `munshi retry <session-id>` — the successful real summary replaces the
placeholder as the next revision.

Related failure categories you may see instead of `summary-failed`, all in the same
"processing attempt errored, safe to retry" family: `transcript-unresolved` (see below),
`source-changed` / `source-incomplete` (the transcript file was being written to mid-read; just
retry), `archive-write-failed` (couldn't write the Markdown file — check disk space and output
directory permissions), and `archive-git-*` (only relevant if you registered with
`--archive-git-history`; a busy or misconfigured archive Git repository).

Three source-read categories are **not** in that family — they park the session permanently
rather than scheduling retries, because retrying cannot help until something outside Munshi
changes. Until issue #57 they were one lumped `source-failed`, which once had doctor advising a
cap raise while the transcripts had actually been destroyed; the split keeps park behavior
identical but lets the diagnosis tell the truth:

- `source-missing` — the transcript file vanished from its recorded path. The park lifts once
  the file is readable again (a `munshi retry <id>` then succeeds), and the operator can instead
  declare the loss with [`munshi settle-lost`](#transcript-lost-and-munshi-settle-lost), which
  settles the session as `transcript-lost`.
- `source-oversized` — the transcript exceeds the configured `--max-source-bytes` cap. Parked
  until the cap is raised: re-register with a larger limit and the periodic park re-evaluation
  (or the next `retry-all`) lifts every park whose transcript now fits. `munshi doctor`'s
  `size-cap-parked` check names the flag to raise.
- `source-failed` — the residual I/O category (for example a permissions error), and the legacy
  code still present on rows recorded before the split, which readers keep accepting.

## `transcript-lost` and `munshi settle-lost`

`transcript-lost` is an explicit operator verdict, not a failure category: it says a parked
`source-missing` session's transcript was destroyed and judged unrecoverable, so the doctor
warnings stop while everything Munshi recorded about the session is retained (issue #58).

```bash
munshi sessions --state transcript-lost   # the settled sessions
munshi settle-lost <session-id>           # settle one (add --source to disambiguate)
munshi settle-lost --all-missing          # settle every eligible parked session
```

The verdict lifts automatically: `munshi tick` (and `retry-all`) runs
`reactivate_regrown_lost_transcripts`, which re-queues any settled session whose transcript has
reappeared at its recorded path — a restore from backup, another machine, a vendor tool putting
the file back. See [`docs/user-guide.md`](user-guide.md#munshi-settle-lost) for the full
command semantics.

## "Sessions I never started", with a `summarizer-exhaust` diagnostic

Your summarizer is a coding-agent CLI (Copilot CLI, Claude Code, or similar) that records a
session of its own for every summary Munshi asks for, and those sessions are landing in the
harness home you registered. Munshi then discovers them as new work, so archiving N sessions
creates N more — a self-feeding loop that has, in the field, produced several hundred archived
and uploaded exhaust sessions before anyone noticed (issue #37).

Munshi recognizes a session whose first user message is one of its own summary-request envelopes
and settles it as `not-archive-worthy` with the `summarizer-exhaust` diagnostic, so it is never
summarized, uploaded, or delivered — no billed model calls, no loop. But the guard is a backstop:
the exhaust sessions still accumulate in the harness's own session directory, and seeing this
diagnostic at all means **your summarizer wrapper's isolation is missing or broken**.

```bash
munshi status                                  # last failure: operation=archive-worker code=summarizer-exhaust ...
munshi sessions --state not-archive-worthy     # the refused sessions themselves
```

The tell-tale sign is a `not-archive-worthy` count climbing in step with how many sessions you
archive, with `summarizer-exhaust` as the recorded diagnostic.

The fix is on the wrapper, not on Munshi: point `--summarizer` at
[`contrib/copilot-summarizer.sh`](../contrib/copilot-summarizer.sh) or
[`contrib/claude-summarizer.sh`](../contrib/claude-summarizer.sh) instead of a bare `copilot` or
`claude` binary, or apply the same isolation to your own wrapper — see
[Summarizers that are themselves session-recording harnesses](summarizers.md#4-hazard-summarizers-that-are-themselves-session-recording-harnesses).
Afterwards, delete the accumulated exhaust sessions from the harness home. Nothing needs
retrying: these sessions were correctly refused.

## "Session deferred"

These are not failures — Munshi's policy is to defer rather than ever drop work (see
[ADR 0005](adr/0005-defer-project-policy-and-budgets-never-drop.md)):

| Category | Cause |
| --- | --- |
| `budget-hourly-exceeded` / `budget-daily-exceeded` | The project hit its `--max-calls-per-hour`/`--max-calls-per-day` summarizer budget (defaults 10/50). |
| `concurrency-deferred` | The global `--max-concurrency` (default 2) live-processing limit is full. |
| `project-disabled` | You ran `munshi project disable <dir>`. |
| `project-override-disabled` | A `.munshi.toml` in the project sets `enabled = false`. |
| `project-override-invalid` | A `.munshi.toml` exists but is unparsable or unreadable — Munshi fails closed rather than silently using defaults. Fix the file. (Symlinked or over-64 KiB candidates are skipped entirely instead.) |

Deferred sessions clear on their own: the next recovery sweep, or the hourly/daily window
rolling over, or the concurrency slot freeing up picks them back up automatically. To act
immediately: `munshi project enable <dir>` for a disabled project, fix and re-save
`.munshi.toml` for an override problem, or just wait out a budget/concurrency deferral (retrying
manually doesn't bypass a still-exceeded budget).

## "Completion reason unknown / interrupted for a clean session" (Claude Code)

Claude Code's `SessionEnd` `reason` field reports `"other"` both for a genuinely clean
noninteractive completion (`claude -p`) and for a mid-turn `SIGINT` interruption — the string
alone can't tell them apart. Munshi treats a previously recorded `Stop` event for that session as
the real completion signal and only maps `reason` values `clear`, `logout`, and
`prompt_input_exit` as affirmative completions; everything else, including a perfectly normal
noninteractive run, degrades to the `unknown` completion reason and is still archived correctly.
Seeing `completion_reason: unknown` in a Claude Code archive's front matter for a session you
know ended cleanly is expected, not a bug — see
[phase-0 Claude Code findings](phase-0-claude-code-findings.md) for the full evidence.

## "`transcript-unresolved`"

This means Munshi had a session to archive but couldn't find its transcript file. It happens
when `sessionEnd` (Copilot) arrives with no transcript path and either no earlier `agentStop`
supplied one, or Copilot's version-pinned session-state fallback
(`$COPILOT_HOME/session-state/<id>/events.jsonl`) doesn't resolve safely. Claude Code sessions
always carry an explicit `transcript_path` on both `Stop` and `SessionEnd`, so this is rare there
except when a session is picked up before its transcript file exists yet.

Usually this resolves itself on the next recovery sweep once the transcript becomes resolvable.
As a last resort, archive it manually with an explicit path:

```bash
munshi archive --source <copilot|claude-code> <session-id> \
  --events /absolute/path/to/transcript \
  --project-dir /absolute/path/to/project \
  --output-dir /absolute/path/to/munshi-summaries \
  --summarizer /absolute/path/to/summarizer
```

## Registration problems

- **"noninteractive registration requires `--accept-transcript-processing`"** — a script or CI
  run without a terminal can't answer the `I ACCEPT` prompt. Pass
  `--accept-transcript-processing` explicitly.
- **"registration paths and executables must be absolute"** — every path you pass to `register`
  (`--summarizer`, `--output-dir`, `--state-dir`, `--copilot-home`, `--claude-home`) must be
  absolute, not `~`-relative or relative to your shell's cwd.
- **"refusing an unsafe symlink, file type, or ownership at `<path>`"** — Munshi refuses to
  read or write through a symlink, or a file it doesn't own, at any managed path. This is
  intentional hardening, not a bug; remove the symlink or fix ownership at the reported path.
- **"another Munshi registration operation is active"** — a concurrent `register`/`unregister`
  is running (or crashed while holding the lock). The lock file itself is safely reusable once
  released — the kernel drops the advisory lock if the other process exited — so retry after a
  moment; you do not need to delete anything.
- **"the harness settings file at `<path>` is not a JSON settings object Munshi can merge
  into"** — Claude Code's `settings.json` must parse as a JSON object for Munshi to merge its
  managed `Stop`/`SessionEnd` entries into. Fix or remove whatever made it not a valid JSON
  object (a stray array, invalid syntax, etc.) and re-register.

## State store issues

Munshi's SQLite state (`$MUNSHI_HOME/munshi.db`) is deliberately just a rebuildable operational
cache — it stores no raw transcript and no historical summary body. Your Markdown archives are
the durable record (see [ADR 0004](adr/0004-use-rebuildable-sqlite-operational-state.md)). For
most problems, `munshi doctor` plus `munshi retry`/`retry-all` is the documented path. If the
database itself seems corrupt or stuck, rebuild it from your archives explicitly:

```bash
munshi hook recover --state-dir ~/.munshi --rebuild-state
```

This backs the existing `munshi.db` aside, recreates the schema, and rebuilds current archive
metadata and the structured-summary cache by re-reading your validated Munshi-owned Markdown.
Existing Markdown is never deleted or invalidated by this. Add `--force-retry` in the same call
to also make any failed-but-retryable work immediately eligible again, bypassing normal backoff.
For routine maintenance — as opposed to a rebuild — `munshi tick` is the scheduled/manual entry
point: it runs the same recovery sweep plus the park lifts, upload/delivery retries, and
exhaust-retention pruning, and is what a launchd/cron timer should invoke.

## "My session archived but never uploaded to Patwari"

Archive upload is opt-in and separate from local archival: a session is archived once its Markdown
exists, even if no snapshot reached Patwari. Check in order:

1. `munshi archive-upload status` (add `--json` for the machine contract) — if upload is not
   configured or not enabled, run `munshi archive-upload configure --endpoint <url>` then
   `munshi archive-upload enable`. Enabling does not retroactively upload; run
   `munshi archive-upload backfill` to upload the archived sessions that accumulated while upload
   was off.
2. If the status shows a failed upload, the server was likely unreachable; failed uploads retry
   with backoff on later hooks and `munshi hook recover`, or immediately via
   `munshi archive-upload retry <session-id>` (`--all` for every eligible session, `--force` to
   bypass backoff and revive dead-lettered uploads). A session the status does not list at all was
   archived before upload was enabled — that is what `backfill` scans for.
3. A `transcript-changed` failure means the live transcript gained events between archival and
   upload; the next revision re-archives and uploads the grown transcript, converging on its own.
4. A `skipped` outcome with reason `missing-transcript.jsonl` means munshi has no readable
   transcript for that session and could not find one. A row that records no transcript path (one
   `rebuild-state` reconstructed from its archive Markdown alone) or records one that no longer
   reads is not skipped on that basis alone: munshi first re-derives the path inside the
   *registered* harness home — Copilot's `session-state/<id>/events.jsonl`, Claude Code's
   `projects/*/<session-id>.jsonl` — checks the transcript against its pinned envelope, records the
   recovered path on the session, and uploads the full snapshot in that same run. So the skip means
   the transcript is genuinely gone, its harness home is not registered, or the session is a Codex
   one (rollout files are not named after the session, so there is no safe lookup). Every snapshot
   is self-contained (ADR 0009), so munshi refuses to upload the summary on its own; the summary stays
   durable locally and in Notesmith, and the session uploads in full the moment its transcript is
   readable again. `backfill` also re-uploads sessions whose recorded snapshot is not known to
   carry both `summary.md` and `transcript.jsonl`, so a summary-only snapshot from an older munshi
   gains a complete sibling — the original stays, because Patwari snapshots are immutable.
5. `munshi retrieve <sha256>` failing with "not found" for a hash a summary references usually
   means that session's snapshot has not been uploaded yet — same checks as above. Manually
   archived sessions (`munshi archive`) never upload; only the hook pipeline does. When the
   session's transcript is still on disk, `munshi retrieve <sha256> --local --session <id>`
   redeems the ticket from the transcript directly, without waiting for the upload.

`munshi retrieve` exits with a distinct, stable code per failure class, so scripts can tell the
kinds apart without parsing messages:

| Exit code | Meaning |
| --- | --- |
| 1 | Local I/O or configuration failure (couldn't write `--output`, unreadable config; also a `--local` session whose transcript path is unknown, missing on disk, or unreadable). |
| 2 | Invalid input: a malformed hash, or `--output` names an existing file without `--force`. |
| 3 | No Patwari server configured. |
| 4 | No matching artifact — including, under `--local`, an unknown `--session` or a hash the transcript doesn't contain. |
| 5 | Server unreachable, protocol error, or server-side failure. |
| 6 | Content verification or decompression failed. |
| 7 | The artifact exceeds the download cap (`--max-download-bytes`). |

`munshi restore` uses the same idea with its own table. A finished run always prints its report
before exiting, so a non-zero code here is an accounting outcome, not a lost run:

| Exit code | Meaning |
| --- | --- |
| 0 | Every selected snapshot restored; nothing refused, skipped, or failed. |
| 1 | Local failure: unreadable configuration, restored bytes that could not be written, or a `--resume` write into the harness home that failed. |
| 2 | Invalid input (the archive server rejected `--session`, or `--yes`/`--claude-home` were passed without `--resume`). |
| 3 | No Patwari server configured, or no archive output directory known — pass `--endpoint` and `--output-dir` on an unregistered machine. |
| 4 | Findings: a local file that differs from the archived original was left alone (rerun with `--force`), an artifact was set aside for a reason you did not ask for (past the download cap, or a logical path with no place in the layout), `--session` matched nothing, or a `--resume` placed nothing — an unsupported harness, an unknown harness home, a differing transcript already in the harness home, or a plan you have not confirmed with `--yes`. Artifacts you asked to skip with `--skip-outputs` are *not* findings and do not appear here. |
| 5 | Server unreachable, protocol error, or server-side failure. |
| 6 | Content verification or decompression failed on at least one artifact. |

A restore that reports `refused` wrote nothing for those artifacts: the local file differs from the
archived original, which is usually a local edit you want to keep. Compare them before reaching for
`--force` — the archived original is always retrievable by its hash with `munshi retrieve`.

A `--resume` that reports `"reason": "target-differs"` is a different situation, and `--force` will
not move it: a transcript already in the harness home that differs from the archived one is a live
session the harness continued past the snapshot, and overwriting it would destroy conversation that
was never archived. Read both files (the archived copy sits in `<session-id>.restored/`) and decide
by hand; the harness home is yours, not Munshi's.

## Getting more signal

Every inspection command (`status`, `sessions`, `show`) accepts `--json` for the stable
machine-readable contract — use it when you need the exact field names/values instead of the
human-readable summary, e.g. for scripting a check or filing a precise bug report.

Diagnostics only ever store safe, content-free category strings like the ones in this doc (plus
which operation/session they came from) — never transcript content, never summary content. You
can share `munshi status --json` or `munshi show <id> --json` output without worrying it leaks
session content.

## See also

- [`docs/getting-started.md`](getting-started.md) — build, register, and verify your first
  archived session.
- [`docs/summarizers.md`](summarizers.md) — the summarizer contract in full, with examples.
- [`docs/user-guide.md`](user-guide.md) — day-to-day operation: session states, retries,
  recovery, budgets, project policy, delivery, archive upload.
- [`docs/automatic-archive.md`](automatic-archive.md) — full detail on hooks, recovery, and
  state internals.
- [`docs/phase-0-claude-code-findings.md`](phase-0-claude-code-findings.md) — the evidence
  behind Claude Code's hook behavior, including the completion-reason ambiguity.

## Sessions that can never archive (identity mismatch)

A session whose recorded ID does not belong to the transcript it points at can never archive: the
read fails on the identity check before any content is read, on every attempt. `munshi doctor`
reports these under `id-mismatch-parked`, and `munshi sessions` shows them `failed` with
`source-id-mismatch`.

The known cause is Copilot CLI firing its `agentStop` hook once per **subagent**, passing the
subagent's tool-call ID (`call_…`) alongside the parent session's transcript path (issue #82).
Munshi refuses those at ingest now, so this only affects rows recorded before that fix. The
subagent's work is not lost — it is part of the parent session's transcript, and the parent
archives normally.

List the affected rows:

```bash
munshi purge-mismatched
```

That is a dry run; it prints each session, the transcript it claims, and the session that
transcript actually belongs to. To remove them:

```bash
munshi purge-mismatched --confirm
```

Only sessions that are parked and never produced an archive are ever eligible — a session with a
Markdown record behind it is refused even if its ID looks wrong. Removing a session also removes
its observations and processing attempts; its diagnostics are kept, detached, so the record of what
happened survives the cleanup.
