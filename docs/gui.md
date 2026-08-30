# Desktop addon

The Munshi desktop app is an optional native window over Munshi's archiving backlog. It shows what
the [backlog dashboard](dashboard.md) shows, adds a searchable session list and per-session
actions, and ships the `munshi` command-line tool inside its own bundle so a user can install both
at once. The command line remains the primary way to run Munshi; the app is an addon, and nothing
in the core install depends on it.

## Building it

```bash
./contrib/build-gui.sh
```

That builds the CLI, stages and signs it as a bundle resource, builds the frontend, and produces
the app. Bundles land in `gui/src-tauri/target/release/bundle/` — a `.app` and `.dmg` on macOS, an
AppImage and `.deb` on Linux. Pass `--debug` for an unoptimized build.

Requirements beyond Munshi's own: Node 20+ and npm, and on Linux the usual Tauri 2 system
dependencies (`webkit2gtk-4.1`, `libgtk-3`). Nothing here is needed to build or test the CLI — the
app is a nested Cargo workspace, deliberately `exclude`d from the repo root's, so
`cargo build --workspace` and `cargo test --workspace` never pull in Tauri.

For frontend work, `cd gui && npm run tauri dev` runs the app against a Vite dev server with hot
reload; it falls back to an installed `munshi` since a dev build has no bundle around it.

## Installing the command line

The app ships the CLI at `Contents/Resources/resources/bin/munshi` (macOS) and offers, on first
run, to copy it to `~/.local/bin/munshi` — the path `contrib/dev-deploy.sh` uses and the launchd
plist references, so the app, the harness hooks and the scheduled sweep all agree on one binary.

It is a copy rather than a symlink into the bundle. Capture keeps working when the app is moved or
deleted, the hook paths Munshi writes into harness configuration stay valid across app updates,
and — on macOS — the copy carries the app's stable signing identity, so TCC grants survive
upgrades instead of being orphaned by a changing cdhash. See
[ADR 0014](adr/0014-ship-the-desktop-addon-over-the-same-cli-json-boundary.md).

The app *prefers* an installed `munshi` over its own bundled copy when reading state, because the
installed one is what actually does the capturing. Resolution order is `MUNSHI_BIN`,
`~/.local/bin/munshi`, `PATH`, then the bundled copy. When the bundled and installed versions
differ the app says so and offers to update the installed copy.

`~/.local/bin` is not on every shell's `PATH`; the app says so when it isn't, and adding it is left
to the user.

## What it shows

Stat tiles (archived, queued, failed, and one per enabled remote sink), a "right now" panel of
in-flight sessions, remaining backlog by project, archived counts by source, a stacked
attempt-outcome chart over the last six hours, a read-only configuration panel with any readiness
check that is not `ok`, and a diagnostics tail.

The session list is the main surface: filter by state, search across title, project and session id,
and open any session for its full detail and current summary. Filtering happens over the listing
already collected rather than by re-running `munshi sessions --state …`, so switching views costs
nothing.

## What it can do

Per session, from the detail drawer:

- **Summarize now / Retry / Update summary** — `munshi retry <id> --source <source> --json`. The
  label follows the session's state; the command is the same, and Munshi decides what it means.
- **Force** — the same with `--force`, which bypasses the retry backoff. This is the only way to
  move a session that has been parked after exhausting its attempts.
- **Open summary** — hands the archive Markdown file to the desktop's default handler.
- **Open in Notesmith** — the `notesmith://` deep link from `show --json`, present once a session
  has been delivered.

Across the machine, from the header: **Refresh**, **Run sweep** (`munshi tick --json`, the same
idempotent sweep the launchd job runs), and **Retry all** (`munshi retry-all --json`).

## What it deliberately does not do

**Registration.** `munshi register` discloses that v1 performs no secret redaction and requires the
user to accept that. It also has no `--json` contract. The app shows the exact command to run and
leaves it in the terminal rather than wrapping a consent step in a button.

**Configuration editing.** The `configure`/`enable`/`disable` mutators are prose-output commands
with no contract to drive them through, so the app displays configuration read-only. Budgets,
sinks and per-project opt-outs stay with the CLI.

Both are open to revisit if those commands ever gain `--json` contracts. Neither is worth inventing
a private channel for.

## Where the data comes from

As settled by [ADR 0007](adr/0007-madari-status-and-actions-over-cli-json-only.md) and applied to
this app by [ADR 0014](adr/0014-ship-the-desktop-addon-over-the-same-cli-json-boundary.md), the app
consumes the versioned CLI `--json` contracts only. Each collection round is one `munshi status
--json`, `sessions --json --limit 1000`, `attempts --json --limit 200`, `diagnostics --json --limit
20`, `archive-upload status --json`, and `summary-delivery status --json`. It never opens the state
directory or its SQLite database.

Rounds run every 20 seconds, immediately after any action, and whenever the window regains focus.
Each round is six subprocesses rather than a cheap HTTP poll, which is why the interval is what it
is. The per-session `items[]` arrays the two sink commands return are dropped before the payload
reaches the page — on a mature archive that is thousands of rows the window never draws.

A command that is missing, exits non-zero, times out (25s), or emits unparseable JSON degrades its
own panel and adds an entry to a "degraded sources" banner carrying the exact argument vector, so
the failure can be reproduced in a terminal. A partial snapshot is always served over none. Before
`munshi register` has ever run every contract is valid but empty, so an unregistered machine shows
empty panels and a setup banner, not errors.

## Security model

There is no HTTP server and no port. The dashboard has to publish unauthenticated session metadata
on loopback and defend that with a bind-address check; this app passes the same payload from a Rust
command straight to its own webview over Tauri's IPC, so session identifiers, project names and
summary titles never leave the process.

The webview is granted `core:default` and nothing more — no filesystem, shell, or HTTP plugin.
Everything it can reach is a validated command:

- Reading an archive is bounded at 4 MiB and takes the path from Munshi's own `archive_path` field.
- Opening a target accepts an existing local path or a `https://`/`notesmith://` URL, and refuses
  anything else.
- Every `munshi` invocation is bounded: 25-second deadline, 8 MiB per stream, both pipes drained on
  their own threads, and the child killed rather than waited on at the deadline.

The CSP is `default-src 'self'` with no remote origins; the app loads no fonts, scripts, or styles
from the network.

## Status

Optional addon, not part of the core install. Nothing in `contrib/launchd` starts it, and the CLI
has no reciprocal dependency on it. It is complementary to the two other consumers of the same
boundary: [Madari](adr/0007-madari-status-and-actions-over-cli-json-only.md) shows Munshi status
next to each discovered session, and the [backlog dashboard](dashboard.md) remains the
zero-dependency, browser-based view for a headless or remote machine where a desktop app is not an
option.
