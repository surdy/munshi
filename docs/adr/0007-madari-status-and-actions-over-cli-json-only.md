# Madari surfaces status and actions strictly over the versioned CLI/JSON boundary

Madari (issue #10) displays Munshi's operational state and offers "view summary", "open Notesmith
link", "summarize now", and "retry" actions by shelling out to the standalone `munshi` executable and
parsing its `--json` output. It never opens, reads, or writes Munshi's SQLite state store, and it
never invokes the hidden `hook`/`hook-worker` subcommands. This mirrors how Munshi itself drives
`git` and how Madari drives its own external tools (`git`, `ssh`, `zmx`): shell out to the real
binary rather than embed or reimplement it, and never share a writable database between processes
that can each independently rebuild their own.

Munshi remains fully usable, and fully unaware of Madari, from a terminal. The `munshi` binary is
optional and independently discoverable by Madari (configurable path, or resolved from `PATH`);
Munshi has no reciprocal dependency on Madari and no Madari-specific code path. An absent,
unresolvable, or incompatible `munshi` binary — wrong `schema_version`, a non-zero exit where JSON
was expected, or malformed stdout — is entirely Madari's concern to degrade safely; Munshi's only
obligation is to keep emitting the documented, versioned `--json` contract.

Because Madari (or any other external caller) may query Munshi before the user has ever run `munshi
register` in a given project, every read-only `--json` command must return a valid, empty
`schema_version: 1` contract rather than an I/O error in that state: `sessions`, `status`, `show`,
`retry`, and — fixed for this issue — `delivery status` all now degrade to an empty/disabled report
(`total: 0`, `found: false`, or `settings.enabled: false`) instead of failing when the state
directory has never been registered. `delivery backfill`/`delivery retry` remain hard failures when
unregistered, since those are user-triggered actions with nothing to act on rather than status
queries a UI polls opportunistically.

Session identity crossing the process boundary is `<source>:<session-id>` (`copilot`, `claude-code`,
or `codex`), matching Madari's own agent-session harness ids one-to-one except `claude` ↔
`claude-code`. Munshi does not capture Gemini or OpenCode sessions, so Madari shows no Munshi status
for those two harnesses. The Notesmith deep link (`show --json` → `session.delivery.note_link`, a
`notesmith://app/v/<vault>/<path>` URL) is computed by Munshi and only ever opened by Madari through
its own existing URL-opening/notification boundary — Munshi has no notification mechanism of its
own and does not need one.
