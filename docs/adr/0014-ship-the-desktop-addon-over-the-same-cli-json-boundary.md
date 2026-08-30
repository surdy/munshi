# The desktop addon ships the CLI and drives it over the same versioned CLI/JSON boundary

The Munshi desktop app (`gui/`) displays operational state and offers "view summary", "open
Notesmith link", "summarize now", and "retry" by shelling out to the standalone `munshi`
executable and parsing its `--json` output. It never opens, reads, or writes Munshi's SQLite state
store, and it never invokes the hidden `hook`/`hook-worker` subcommands. This is
[ADR 0007](0007-madari-status-and-actions-over-cli-json-only.md)'s boundary applied to a second
consumer, for the same reasons: the app cannot contend with a running Munshi, cannot corrupt
anything, and cannot drift from the published contracts. Munshi itself grows no UI code path and
stays entirely unaware the app exists.

Unlike Madari and the backlog dashboard, the app *ships* the CLI rather than merely discovering it:
one bundle contains both, and a first-run action copies the bundled binary to `~/.local/bin/munshi`.
The copy is deliberate — a symlink into the bundle would break capture when the app is moved or
deleted, and would tie the hook paths Munshi writes into harness configuration to the app's
lifetime. It is a copy, not a second source of truth: the app *prefers* whatever is installed over
its own bundled binary when reading contracts, because the installed copy is the one the harness
hooks and the launchd tick actually execute. Reading from anything else would report a machine
other than the one the user has. When the two versions differ the app says so and offers to update
the installed copy; it never silently switches.

On macOS the staged CLI is signed with the same stable identity `contrib/dev-deploy.sh` uses,
before the app is bundled around it. The launchd tick runs `munshi` in the background, where TCC
attributes folder access to the binary and keys the grant to its designated requirement; an ad-hoc
binary has none, so TCC falls back to a cdhash that changes on every rebuild and re-prompts.
Signing the copy that gets installed is what keeps those grants alive across app upgrades.

The app is a nested Cargo workspace, `exclude`d from the repo root's. Munshi's own
`cargo build --workspace` and `cargo test --workspace` must not pull in Tauri, and a contributor on
a bare Linux box must not need webkit2gtk to build and test the CLI. The addon is optional in the
same sense `munshi-dashboard` is: nothing in the core install references it, and nothing in
`contrib/launchd` starts it.

Two things the app deliberately does *not* do. It does not run `register`: that command carries a
disclosure the user must read and accept, and it has no `--json` contract to drive it through, so
the app shows the exact command and leaves it in the terminal. And it does not edit `config.json`,
for the same contract reason — the `configure`/`enable`/`disable` mutators are prose-output
commands, so configuration is displayed read-only. Both remain open to a later ADR if those
commands gain contracts; neither is worth inventing a private channel for.

The app has no HTTP server and binds no port. `munshi-dashboard` had to publish unauthenticated
session metadata on loopback and defend that with a bind-address check; the desktop app passes the
same payload from a Rust command to its own webview over Tauri's IPC, so session titles and project
names never leave the process. The webview is granted `core:default` and nothing else: no
filesystem, shell, or HTTP plugin. Reading an archive file and opening a link are validated
commands — the reader is bounded and refuses anything larger than a summary, and the opener accepts
an existing local path or the two URL schemes Munshi actually emits, and nothing else.
