# Using Munshi day to day

This guide assumes you have already worked through
[`getting-started.md`](getting-started.md): Munshi is registered, its hooks are installed, and at
least one session has archived successfully. It covers the commands and concepts you'll reach for
once Munshi is just running in the background.

## The mental model

Munshi's hooks passively observe your coding-agent sessions (Copilot, Claude Code, or both) as they
happen; Munshi reads only lifecycle metadata from hook payloads and never logs or persists
conversation content from them. Each observed event is
queued as work in a small SQLite state store — Munshi's operational bookkeeping, not your data. A
detached worker picks up queued work, sends the new transcript content to your configured summarizer,
and writes or updates exactly one Markdown file per logical session. A session you resume later
isn't a new archive: it becomes the next *revision* of the same file, with the summarizer given the
previous summary plus only the new delta. Nothing is ever silently dropped — if a budget, a
concurrency limit, or a disabled project blocks a session, it is deferred and retried later, not
abandoned. The Markdown under your output directory is the durable, human-readable record of
everything Munshi has archived; the SQLite database is disposable operational state that can always
be rebuilt from that Markdown.

## Checking on things

### `munshi status`

The fastest overall health check: registration state, whether hooks are installed, and rollups of
pending/failed/archived work.

```bash
munshi status
munshi status --json   # stable machine-readable contract
```

### `munshi sessions`

Lists individual sessions and their current operational state.

```bash
munshi sessions
munshi sessions --state failed
munshi sessions --limit 100 --json
```

| State | Meaning |
| --- | --- |
| `archived` | Fully archived; the Markdown file reflects the latest known content. |
| `summary-pending` | Observed, archive-worthy, and queued for its first summary. |
| `revision-pending` | Already archived at least once; new content is queued for the next revision. |
| `interrupted` | Ended without a clean completion signal (e.g. force-killed); queued to archive with an interrupted completion reason. |
| `failed` | The last processing attempt errored; retryable once backoff passes. |
| `delivery-related` | Local archival is done; what's pending or failed is remote delivery to Notesmith. |
| `disabled-project` | The owning project is disabled (explicitly or via `.munshi.toml`), so processing is on hold. |
| `processing` | A worker currently holds this session's lock and is actively summarizing it. |
| `observed` | A hook has seen the session, but nothing has queued it for summarization yet. |
| `not-archive-worthy` | The transcript never reached the minimum bar (a user request plus agent content or tool activity); it will not be archived. Covers hook-observed sessions and stubs the recovery sweep judged; if the transcript later grows a real reply, the session is picked up again and can archive. |
| `transcript-lost` | The operator settled the session with `settle-lost`: its transcript was destroyed and judged unrecoverable. Everything munshi recorded about the session is retained, and if a transcript reappears at its recorded path the verdict lifts automatically on the next `retry-all`. |
| `unknown` | Doesn't cleanly map to any state above (rare; treat as diagnostic). |

`--state` filters to one state; `--json` emits the stable machine-readable contract; `--limit`
caps how many rows come back (default 50).

### `munshi show <id>`

Shows one session's metadata and current summary. If the same session ID exists under more than
one source (e.g. you registered both Copilot and Claude Code), Munshi refuses to guess and asks you
to disambiguate:

```bash
munshi show 11111111-1111-4111-8111-111111111111
munshi show 11111111-1111-4111-8111-111111111111 --source claude-code
munshi show 11111111-1111-4111-8111-111111111111 --json
```

### `munshi attempts`

Lists recent processing attempts — the per-run outcomes behind each session's current state
(succeeded, failed, recovered, superseded) with their error categories and timestamps. Useful for
spotting failure patterns that `sessions` (which shows only each session's latest state) folds away.

```bash
munshi attempts
munshi attempts --limit 200 --json
munshi attempts --since-ms 1785400000000   # attempts active at/after this Unix-ms instant
```

### `munshi diagnostics`

Shows the tail of Munshi's diagnostics ledger: operation, category, and cause for recorded
anomalies (parked evidence, schema drift fallbacks, worker errors). The deeper companion to the
single `last failure` line in `status`.

```bash
munshi diagnostics
munshi diagnostics --limit 50 --json
```

### `munshi settle-lost`

Declares destroyed transcripts lost — an explicit operator action, never automatic. Eligible
sessions are permanently parked under a missing-source failure (`source-missing`, or the older
lumped `source-failed`) with no file at their recorded transcript path. Settled sessions show as
`transcript-lost`, leave the doctor warnings, and reactivate on their own if the file comes back.

```bash
munshi settle-lost <session-id>      # one session; exits non-zero if it did not settle
munshi settle-lost --all-missing     # every eligible session
```

A parked session whose transcript still exists is refused (`transcript-present`) — that park is a
size cap, and raising the limit is the fix.

### `munshi tick`

One idempotent maintenance sweep, made for platform schedulers (issue #55): the recovery sweep
a hook event would run, park and lost-verdict re-evaluation, and the eligible upload/delivery
retries. It prints nothing when there is nothing to do and is a silent no-op on an unregistered
machine, so a timer can fire it forever without conditioning on state. Hook events already run
most of this on a busy machine — the tick exists for the *idle* one, where parked retries would
otherwise wait for the next session.

```bash
munshi tick
munshi tick --json
```

`contrib/launchd/com.munshi.tick.plist` runs it every 15 minutes on macOS; see the comment in
the plist for install/remove commands (systemd user timers are the Linux equivalent).

### `munshi doctor`

Diagnoses registration, dependencies, and runtime readiness — useful after upgrading Munshi, moving
machines, or when something feels off but `status` doesn't explain it.

```bash
munshi doctor
```

### `munshi configuration-check`

Validates the current registration and configuration contracts (hook files, config file, state
directory) without touching any session state. Good to run after manually editing anything under
your Munshi home.

```bash
munshi configuration-check
```

## Retrying work

Most stuck sessions resolve themselves on the next hook-triggered recovery sweep. When you don't
want to wait, retry explicitly:

```bash
munshi retry <session-id>
munshi retry <session-id> --source claude-code
munshi retry <session-id> --force
munshi retry-all
munshi retry-all --force --limit 32
```

`retry` re-runs one session through the normal worker state machine; `retry-all` does the same for
every currently eligible session (default limit 32). `--source` disambiguates a session ID that
exists under more than one harness, same as `show`. `--force` is only needed for `failed` sessions
that haven't yet passed their retry backoff, or for sessions carrying a permanent-retry marker —
without it, a still-backed-off failure is simply skipped. You never need `--force` for
budget-deferred or concurrency-deferred sessions; those clear on their own once the constraint that
deferred them resolves (see below), and a plain `retry`/`retry-all` or the automatic recovery sweep
picks them up.

A session whose summarizer keeps rejecting its input deterministically — it reached the
repeat-failure park, or no split can bring its request under `chunk_threshold_bytes` (its own
`summary-input-limit` category) — does not stay unarchived: Munshi archives it with an explicit
placeholder summary so the full transcript still reaches the durable archive and uploads to
Patwari (issue #43). The archive file is flagged `summary_placeholder: true` and tagged
`munshi-placeholder-summary`, `munshi status` counts these sessions as `placeholder=<n>` on its
sessions line, and the session stays parked because a real summary is still owed. A plain
`munshi retry <session-id>` re-attempts a real summary; when it succeeds, the real summary
replaces the placeholder as the next revision and is re-uploaded and re-delivered automatically.

## Marathon sessions and chunked summaries

Very long "marathon" sessions rarely need the placeholder above: they are summarized in chunks
instead (issue #48). This section describes what actually happens, what it costs, and how to
steer it — all of it automatic, with nothing to enable.

**When it triggers.** Before summarizing, Munshi builds the real one-shot summarizer request and
measures it. If the measured request exceeds `chunk_threshold_bytes` (default 2.5 MiB,
calibrated against real summarizer-backend rejections), the session takes the chunked path
instead of one shot.

**What happens.** Munshi splits the session's normalized events — only ever on event boundaries,
never mid-event — into segments of roughly `chunk_size_bytes` (default 1.5 MiB) each. It then
makes one summarizer call per segment, in order, giving each call the previous segment's summary
as running continuity context, and finally one *reduce* call that merges the segment summaries
into the single summary that gets archived. In the rare case where even the merged segment
summaries are too large for one reduce call, Munshi condenses them through intermediate reduce
calls and reduces again — recursion, bounded by the same threshold.

**What it costs.** Every chunk and reduce call is charged against the per-project budget
individually, exactly like a normal summary call: a marathon session costs about
`ceil(input / chunk_size_bytes) + 1` calls. If you archive marathons routinely, size
`--max-calls-per-hour`/`--max-calls-per-day` with that multiplier in mind.

**When it fails.** A failure in any chunk or reduce call abandons the whole attempt — partial
summaries are never archived — and the session goes through the completely normal
backoff/retry/park machinery described above. The placeholder floor remains the last resort for
what chunking genuinely cannot fix: a request no split can bring under the threshold (for
example one enormous un-elided event) floors immediately under `summary-input-limit`.

**How to see that it happened.** The extra calls show up in the per-project budget, and that is
all: the archived file is a completely normal summary, and nothing user-visible marks it as
having been produced in chunks — deliberately, since the chunking is an implementation detail of
*how* the summary was produced, not part of the durable record. See
[`summarizers.md`](summarizers.md) for the request contract and
[`configuration.md`](configuration.md) for the two `chunk_*` limits.

### Per-phase summarizer models

Chunk passes are numerous and mechanical; the final reduce is where quality matters most. Both
contrib wrappers therefore let you pay for a cheaper model on chunk passes and a better one on
the reduce, selected by two wrapper-contract variables — and you can set those in Munshi's own
configuration at registration, without touching the ambient environment:

```bash
munshi register ... \
  --summarizer-env MUNSHI_CHUNK_MODEL=<model> \
  --summarizer-env MUNSHI_REDUCE_MODEL=<model>
```

`--summarizer-env KEY=VALUE` (repeatable) is stored as `summarizer.env` in `config.json` and
exported on every summarizer invocation. Munshi itself treats the map as opaque — the *wrapper*
consumes these particular keys (see [`summarizers.md`](summarizers.md)). Copilot CLI accepts
model names and `auto` for its `--model` flag; when neither variable is set, both phases use
your account's default model.

## Recovery

Hooks are the normal path, but a force-killed or crashed session emits no `SessionEnd`/`Stop` event
at all. Munshi covers this two ways:

- **Automatic sweep.** Every hook invocation (`agentStop`/`sessionEnd` for Copilot, `Stop`/
  `SessionEnd` for Claude Code) opportunistically spawns a recovery pass in the background. It
  finds sessions with no end event once both their hook activity and transcript file have gone
  quiet, and — for Claude Code specifically — also scans the registered Claude home's
  `projects/*/` directories for stale, unrecognized `<session-id>.jsonl` transcripts left behind by
  a hookless crash, and archives them too.
- **Manual.** If you don't want to wait for another hook to fire, `munshi retry <id>` or
  `munshi retry-all` claims the same pending/interrupted work directly.

Either path archives the session with an `interrupted` (or, if the reason genuinely can't be
determined, `unknown`) completion reason rather than losing it.

## Controlling scope and cost

### Per-project enable/disable

```bash
munshi project disable /absolute/path/to/project
munshi project enable /absolute/path/to/project
munshi project status /absolute/path/to/project
```

Project identity follows the normalized Git remote (or a local fallback hash), so disabling a
project follows its clones and worktrees. Disabling only stops *future* processing and delivery —
it never deletes anything already archived or delivered. Re-running `munshi register` (say, to
change budgets) never silently re-enables a project you disabled; the `disabled_projects` list is
preserved across registration.

### Budgets

Registration also sets global cost controls, stored in `$MUNSHI_HOME/config.json`:

- `--max-calls-per-hour` (default 10) and `--max-calls-per-day` (default 50): summarizer
  invocations allowed per project, on a rolling window.
- `--max-concurrency` (default 2): sessions summarized concurrently across *all* projects.

Re-run `munshi register` with new values to change them. Every `config.json` setting — these
budgets, limits, `summary_delivery`, `archive_upload`, and the rest — is documented in
[`configuration.md`](configuration.md).

### `.munshi.toml` project overrides

Drop a `.munshi.toml` in (or above) a project to override policy for just that project. Munshi
discovers it by walking upward from the session's origin directory, nearest file wins — the same
way `.editorconfig` or `.git` are discovered:

```toml
[project]
enabled = false
max_calls_per_hour = 2
max_calls_per_day = 10
max_input_bytes = 4000000
timeout_ms = 120000
```

An overridden `max_input_bytes` should stay at or above the global `chunk_threshold_bytes` for the
same reason `register` enforces that relation
([`configuration.md`](configuration.md#the-size-knob-relation)); the override file is read at hook
time and is not validated for you.

Every field is optional and falls back to global configuration when absent. Precedence is: nearest
`.munshi.toml`, then global config, then built-in defaults. `enabled = false` here disables the
project the same way `munshi project disable` does — but the explicit `disabled_projects` list
always wins over an override that says `enabled = true`. An unparsable or unreadable override fails
closed: the project is treated as disabled (`project-override-invalid`) rather than silently
processing with default settings, until you fix the file. A symlinked or over-64 KiB candidate is
not trusted at all — it is skipped and the upward search simply continues past it.

### Nothing is dropped, only deferred

Hitting a budget or concurrency limit never discards a session — it leaves it in its current
pending state with a diagnostic category (`concurrency-deferred`, `budget-hourly-exceeded`,
`budget-daily-exceeded`, `project-disabled`, `project-override-disabled`, or
`project-override-invalid`) and picks it back up automatically once the constraint clears: a worker
slot frees up, the hourly/daily window rolls over, or the project is re-enabled. You'll see these
categories in `munshi show <id>` while a session is waiting.

## Archive layout and identity

Every session's durable identity is `<source>:<session-id>` (for example
`copilot:11111111-1111-4111-8111-111111111111` or `claude-code:9f8e...`), which keeps same-ID
sessions from different harnesses from ever colliding. On disk, under your registered output
directory:

- Copilot keeps its original flat layout: `<project-component>/<session-id>.md`.
- Every other source (Claude Code, Codex) nests under a source segment:
  `<project-component>/<source>/<session-id>.md`.

Each file's YAML frontmatter is worth skimming when you're scripting against archives or just
curious:

- `id` — the full `<source>:<session-id>` identity.
- `agent` — the harness label (`copilot-cli`, `claude-code`, `codex-cli`).
- `project_identity` — the normalized project identity archival and disabling are keyed on.
- `summary_revision` — increments each time a resumed session is re-summarized.
- `completion_reason` — how the session ended (clean, interrupted, unknown, etc.).

If you registered with `--archive-git-history`, your output directory is also a dedicated Git
repository with one commit per successful summary revision, so `git log` on it doubles as a change
history for your own work.

## Multiple machines and harnesses

Munshi keeps one state home per machine (`$MUNSHI_HOME`, default `~/.munshi`), shared by every
harness registered on that machine. Registering both Copilot and Claude Code at once is expected —
`munshi register` with no `--harness` flag targets every harness whose home directory exists, and
both share the same state store, summarizer configuration, and archive tree. `munshi sessions`/
`munshi status` show sessions from both sources side by side, distinguished by their
`<source>:<id>` identity. Different machines each keep their own independent state home and
archive tree; there's no built-in sync between them.

## Remote delivery (Notesmith) in brief

Summary delivery to a Notesmith vault is disabled by default and, when enabled, is strictly
downstream of local archival — it never blocks or rolls back a local Markdown write. The
essentials (`munshi delivery` still works as a deprecated alias for `munshi summary-delivery`):

```bash
munshi summary-delivery configure --endpoint http://127.0.0.1:27183 --vault my-vault
munshi summary-delivery enable
munshi summary-delivery status
munshi summary-delivery backfill              # dry run by default
munshi summary-delivery backfill --confirm    # actually publish
munshi summary-delivery retry --all
munshi summary-delivery retry <session-id> --force
```

`configure` records the sink without turning it on; `enable` reports how many existing summaries
are pending backfill; `disable` stops future delivery while keeping history. `backfill` publishes
existing archives (dry run unless `--confirm`); `retry` retries failed deliveries (`--force`
revives dead-letter sessions). Full design and rationale:
[`automatic-archive.md`](automatic-archive.md) and
[ADR 0006](adr/0006-deliver-to-notesmith-downstream-of-local-archival.md).

## Archive upload (Patwari) in brief

Archive upload publishes each summary revision's full snapshot — the rendered summary, the verbatim
transcript, and any extracted outputs — to a Patwari archive server. Like delivery, it is disabled
by default and strictly downstream of local archival: a Patwari outage never blocks or rolls back a
local Markdown write, and it runs in parallel with, and independently of, Notesmith delivery.

```bash
munshi archive-upload configure --endpoint http://127.0.0.1:8080
munshi archive-upload enable
munshi archive-upload status
munshi archive-upload retry --all
munshi archive-upload retry <session-id> --force
munshi archive-upload backfill
```

`configure` records the server without turning upload on; `enable` requires a configured server and
turns upload on; `disable` stops future upload while keeping upload history. `status` shows the
configuration and per-session upload state. `retry` re-attempts failed uploads (`--force` revives
dead-letter sessions and resets their bounded attempt count). Uploads whose backoff has elapsed are
also retried automatically by the recovery sweep (`munshi hook recover`), so a transient outage
recovers without a new revision. `backfill` uploads archived sessions the configured server holds
no self-contained snapshot for — sessions archived while upload was disabled or unconfigured, and
sessions whose recorded snapshot is not known to carry both `summary.md` and `transcript.jsonl` —
running each through the normal upload path (`--limit` bounds one run, default 200).

Every snapshot is self-contained by contract, so a session whose transcript munshi cannot read is
reported `skipped` rather than uploaded as a summary-only snapshot. Before skipping, munshi tries to
find the transcript: a session whose row has no transcript path (one reconstructed by
`rebuild-state` from its Markdown alone) or whose recorded path no longer reads is looked up inside
the registered harness home — Copilot by its `session-state/<id>/events.jsonl` layout, Claude Code
by scanning `projects/*/` for `<session-id>.jsonl` — and, once the file matches that harness's
pinned envelope, the path is recorded on the session and the full snapshot uploads in the same run.
Only a transcript that is genuinely gone (or a Codex session, whose rollout files are not named
after the session) stays `skipped`; it uploads normally once the transcript is readable again.
Snapshots recorded before munshi tracked which artifacts it
uploaded are re-verified by the first `backfill` after upgrading: re-uploading an identical set
transfers nothing (Patwari deduplicates blobs by content hash and coalesces the identical snapshot
fingerprint), and a snapshot that really was summary-only gains a complete sibling. The incomplete
snapshot itself is never rewritten — Patwari snapshots are immutable, so it stays as historical
provenance of what was captured then. Once a snapshot is uploaded, `munshi retrieve <sha256>` redeems a
claim ticket for the original content (`--max-download-bytes` raises the 128 MiB per-artifact
download cap for a deliberately large artifact). Full design and rationale:
[ADR 0009](adr/0009-archive-full-snapshots-to-patwari.md) and
[ADR 0010](adr/0010-elide-with-claim-tickets-retrieve-on-demand.md).

### `munshi verify-archive-parse`

The read-time acceptance check ([ADR 0011](adr/0011-interpret-transcripts-at-read-time-through-a-shared-streaming-crate.md)/
[0012](adr/0012-defer-the-analysis-client-until-a-first-consumer-exists.md)): walks the Patwari
archive, downloads and hash-verifies each snapshot's transcript, stream-parses it with the shared
interpreter, and reports per-session accounting — records seen, share typed, deliberately-ignored
kinds, `Unknown` kinds with bounded samples, and record errors.

```sh
munshi verify-archive-parse --all
munshi verify-archive-parse --session <patwari-session-id>
```

Exit `0` means every transcript downloaded, verified, and parsed with zero unknowns and zero
errors; distinct non-zero codes separate findings, verification failures, transport failures, and
configuration problems (see the module docs' table, and `--json` for the machine report). Run it
manually after a harness format bump or new-adapter support — Unknown kinds are interpretation
gaps to type, not noise.

## Unregistering and cleanup

```bash
munshi unregister
```

This removes only what Munshi positively recognizes as its own: the dedicated hook installations
(Copilot's `hooks/munshi.json`, or Munshi's managed entries inside Claude Code's `settings.json`)
and the active configuration that drives new processing. It does **not** touch your archived
Markdown, your archive Git history, or the operational state already recorded in SQLite — all of
that remains in place, so re-registering later picks up where you left off rather than starting
over.

## Where to go next

- [`getting-started.md`](getting-started.md) — first-time registration and setup.
- [`configuration.md`](configuration.md) — every `config.json` setting in one place.
- [`summarizers.md`](summarizers.md) — choosing and configuring a compatible summarizer.
- [`troubleshooting.md`](troubleshooting.md) — diagnosing failures beyond what `doctor` and
  `configuration-check` cover.
