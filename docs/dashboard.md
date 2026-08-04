# Backlog dashboard addon

`munshi-dashboard` is an optional local web dashboard over Munshi's archiving backlog,
adopted as a supported addon in issue #56. It replaced the Python spike that was rescued
into `contrib/dashboard` (previously served on `127.0.0.1:8877`), keeping the spike's page
and its `/api/data` payload shape unchanged. It is a separate binary in this workspace;
the `munshi` CLI itself grows no UI code path.

## Running it

```bash
cargo build --release -p munshi-dashboard
./target/release/munshi-dashboard
```

The binary runs in the foreground until interrupted — there is no daemon, no state of its
own, and nothing to clean up. Two flags exist:

- `--bind <addr>` (default `127.0.0.1:8877`) — the address to listen on. Only loopback
  addresses are accepted; see [Security model](#security-model).
- `--munshi <path>` (default `munshi`) — the `munshi` executable every figure is read
  from. A bare name is resolved against `PATH` on every invocation, so upgrading the
  installed binary needs no dashboard restart.

Open `http://127.0.0.1:8877/` in a browser. The only other route is `/api/data`, the JSON
snapshot the page polls; everything else (including any method other than `GET`) is 404.

## What it shows

Stat tiles (archived/total, queued, failed, uploads, deliveries), a "right now" panel of
in-flight sessions, remaining backlog by project, archived counts by source, a stacked
attempt-outcome chart over the last six hours, and recently-archived and recent-failure
tables with a diagnostics tail. The spike's backlog-driver and archiving-rate sections
(burndown chart, rate and ETA tiles) render permanently in their absent states: the
driver and its log are gone, so there is nothing to derive them from.

## Where the data comes from

As settled by [ADR 0007](adr/0007-madari-status-and-actions-over-cli-json-only.md), the
dashboard consumes the versioned CLI `--json` contracts only. Each snapshot is one round
of `munshi status --json`, `archive-upload status --json`, `summary-delivery status
--json`, `sessions --json --limit 1000`, `attempts --json --limit 200`, and `diagnostics
--json --limit 5`. It never opens the state directory or its SQLite database — the
spike's DB-copy deviation noted in issue #56 is gone — so it cannot contend with a
running Munshi, cannot corrupt anything, and cannot drift from the published contracts.
The payload's `db` section keeps the spike's key name but is now assembled from the
`sessions`, `attempts`, and `diagnostics` contracts.

Consequences: the dashboard is strictly read-only; it needs a working `munshi` binary
(pre-registration, the contracts are valid but empty, per ADR 0007), not access to `MUNSHI_HOME`. A command
that is missing, fails, times out, or emits unparseable JSON degrades its own section and
adds a banner entry — a partial snapshot is always served over no snapshot. Snapshots are
cached for 30 seconds and the page polls every 45, so one open tab costs roughly one
round of `munshi` invocations per poll.

## Security model

The snapshot carries session identifiers, project names, and summary titles, and nothing
in the server authenticates a caller. Binding is therefore restricted to loopback
addresses: `--bind 0.0.0.0:8877` (or any routable address) is refused before the socket
is opened rather than documented as a hazard. If you need remote access, tunnel it
(e.g. `ssh -L`).

## Status

Supported optional addon, not part of the core install: nothing in `contrib/launchd`
starts it, and it is intended to be run on demand. Issue #56 deliberately left open
whether a per-session surface also lands in Madari's Munshi integration (#10) —
complementary, if so — and places archive-side, cross-client views with Patwari's
deferred browser interface, not this tool.
