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
| `not-archive-worthy` | The transcript never reached the minimum bar (a user request plus agent content or tool activity); it will not be archived. |
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

Re-run `munshi register` with new values to change them.

### `.munshi.toml` project overrides

Drop a `.munshi.toml` in (or above) a project to override policy for just that project. Munshi
discovers it by walking upward from the session's origin directory, nearest file wins — the same
way `.editorconfig` or `.git` are discovered:

```toml
[project]
enabled = false
max_calls_per_hour = 2
max_calls_per_day = 10
max_input_bytes = 500000
timeout_ms = 120000
```

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

Delivery to a Notesmith vault is disabled by default and, when enabled, is strictly downstream of
local archival — it never blocks or rolls back a local Markdown write. The essentials:

```bash
munshi delivery configure --endpoint http://127.0.0.1:27183 --vault my-vault
munshi delivery enable
munshi delivery status
munshi delivery backfill              # dry run by default
munshi delivery backfill --confirm    # actually publish
munshi delivery retry --all
munshi delivery retry <session-id> --force
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
recovers without a new revision. `backfill` uploads archived sessions the configured server has no
upload record for — sessions archived while upload was disabled or unconfigured — running each
through the normal upload path (`--limit` bounds one run, default 200). Once a snapshot is uploaded, `munshi retrieve <sha256>` redeems a
claim ticket for the original content (`--max-download-bytes` raises the 128 MiB per-artifact
download cap for a deliberately large artifact). Full design and rationale:
[ADR 0009](adr/0009-archive-full-snapshots-to-patwari.md) and
[ADR 0010](adr/0010-elide-with-claim-tickets-retrieve-on-demand.md).

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
- [`summarizers.md`](summarizers.md) — choosing and configuring a compatible summarizer.
- [`troubleshooting.md`](troubleshooting.md) — diagnosing failures beyond what `doctor` and
  `configuration-check` cover.
