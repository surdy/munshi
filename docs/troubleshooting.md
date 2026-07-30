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
   behavior, not a bug, and there is nothing to retry.
3. **Force-killed sessions arrive later, not never.** `SIGKILL` (or any hard crash) during an
   active turn emits no hook at all — there's no event to fail open from. Every hook invocation
   opportunistically kicks off a background recovery sweep that, after a quiet period, finds
   stale sessions with no end event (and, for Claude Code, also scans
   `~/.claude/projects/*/` for orphaned `<session-id>.jsonl` transcripts) and archives them with
   an `interrupted` completion reason. Give it a few minutes; if you don't want to wait, run
   `munshi retry <id>` or `munshi retry-all` directly, or force the sweep now (use your actual
   state directory — `~/.munshi` unless you set `$MUNSHI_HOME`):
   ```bash
   munshi hook recover --state-dir ~/.munshi --stale-after-ms 1800000
   ```
4. **Check `munshi doctor`'s hook checks.** `claude-hook-contract` (or `hook-contract` for
   Copilot) failing usually means the hooks aren't installed the way Munshi expects — see
   Registration problems below.
5. **A moved or rebuilt binary breaks the hook path.** Registration bakes the *absolute path* of
   the `munshi` binary you ran `register` with into the hook entries. If you rebuild or relocate
   that binary, existing hooks still point at the old path and silently no-op. Re-run
   `munshi register` after moving/rebuilding the binary.

## "Session failed with `summary-failed`"

`summary-failed` means your configured summarizer executable was invoked but its output (or
exit) didn't satisfy Munshi's contract. See [`docs/summarizers.md`](summarizers.md) for the full
contract; the common causes are:

- The process printed something other than exactly one JSON object matching the required
  eight-field schema (a Markdown fence, leading commentary, an extra field, an empty required
  list instead of a placeholder like `"none"`).
- The process exited nonzero.
- The process ran past `--timeout-ms` (default 300000ms) and was killed.
- stdout exceeded `--max-stdout-bytes` (default 262144) or stderr exceeded `--max-stderr-bytes`
  (default 65536).
- The normalized transcript itself exceeded `--max-input-bytes` (default 8388608) — this
  produces a summary failure before your summarizer is even invoked.

Test the summarizer standalone before blaming Munshi:

```bash
cat sample-request.json | /absolute/path/to/your-summarizer | python3 -m json.tool; echo "exit=$?"
```

If that doesn't cleanly print all eight fields, fix the summarizer (or its wrapper — see
`contrib/claude-summarizer.sh` for a reference fix that strips Markdown fences and backfills
empty lists) before retrying Munshi. Once it's fixed:

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
5-failure park, and an input over `--max-input-bytes`, which fails immediately under its own
`summary-input-limit` category. Both archive the session with a machine-generated placeholder
summary (frontmatter `summary_placeholder: true`, tag `munshi-placeholder-summary`) so the
transcript still uploads and delivers; the session stays parked and is counted as
`placeholder=<n>` on the `munshi status` sessions line. Fix the summarizer (or raise the input
limit) and run a plain `munshi retry <session-id>` — the successful real summary replaces the
placeholder as the next revision.

Related failure categories you may see instead of `summary-failed`, all in the same
"processing attempt errored, safe to retry" family: `transcript-unresolved` (see below),
`source-changed` / `source-incomplete` (the transcript file was being written to mid-read; just
retry), `archive-write-failed` (couldn't write the Markdown file — check disk space and output
directory permissions), and `archive-git-*` (only relevant if you registered with
`--archive-git-history`; a busy or misconfigured archive Git repository).

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
   transcript for that session — typically one `rebuild-state` reconstructed from its archive
   Markdown alone, or one whose harness transcript has since been removed. Every snapshot is
   self-contained (ADR 0009), so munshi refuses to upload the summary on its own; the summary stays
   durable locally and in Notesmith, and the session uploads in full the moment its transcript is
   readable again. `backfill` also re-uploads sessions whose recorded snapshot is not known to
   carry both `summary.md` and `transcript.jsonl`, so a summary-only snapshot from an older munshi
   gains a complete sibling — the original stays, because Patwari snapshots are immutable.
5. `munshi retrieve <sha256>` failing with "not found" for a hash a summary references usually
   means that session's snapshot has not been uploaded yet — same checks as above. Manually
   archived sessions (`munshi archive`) never upload; only the hook pipeline does.

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
