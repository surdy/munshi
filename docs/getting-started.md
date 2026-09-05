# Getting started with Munshi

Munshi is a local-first session archivist for coding-agent CLIs. When you finish a session in
GitHub Copilot CLI or Claude Code, Munshi shells out to a CLI you configure to produce a
structured summary, then writes a durable local Markdown archive of what happened — no daemon,
no server, no cloud storage. This guide walks you from a fresh clone to automatic archiving of
your everyday sessions.

## 1. Install Munshi

Munshi ships as a Rust workspace with no packaged releases yet, so you build it yourself. There
are two ways to do that, and they end in the same place: a `munshi` binary on your `PATH` at
`~/.local/bin`. Pick either — the rest of this guide is identical afterwards.

The command line is the primary way to run Munshi. The desktop app is an optional addon that
happens to be the easiest way to *install* the command line, because it ships it inside its own
bundle and handles the codesigning wrinkle below for you.

### Option A — the desktop app (also installs the command line)

Builds the CLI, signs it, and wraps both in an app bundle:

```bash
./contrib/build-gui.sh
```

Open the result from `gui/src-tauri/target/release/bundle/` (a `.app` or `.dmg` on macOS, an
AppImage or `.deb` on Linux), and click **Install** on the banner it shows on first run. That
copies the bundled `munshi` to `~/.local/bin/munshi`.

Needs Node 20+ and npm on top of Munshi's own requirements, plus `webkit2gtk-4.1` and `libgtk-3`
on Linux. Full details in [the desktop addon guide](gui.md).

### Option B — the command line only

```bash
cd /absolute/path/to/munshi
cargo build --release
```

The binary lands at `target/release/munshi`. Registration (below) records the **absolute path**
of this binary inside your hook files, so put it somewhere stable before registering — copy it
onto your `PATH`, preferably `~/.local/bin`, which is the path the bundled launchd plist
([`contrib/launchd/com.munshi.tick.plist`](../contrib/launchd/com.munshi.tick.plist)) and
[`contrib/dev-deploy.sh`](../contrib/dev-deploy.sh) both assume:

```bash
mkdir -p ~/.local/bin && cp target/release/munshi ~/.local/bin/munshi
```

If you ever move or rebuild the binary at a new path, re-run `munshi register` afterward so the
hooks point at the right place.

### One macOS wrinkle, either way

On macOS there's one more thing worth knowing before you schedule anything in the background:
TCC attributes folder-access grants (Documents, Desktop, …) for a background `munshi` to the
binary itself, keyed to its codesigning identity — and an ad-hoc-signed binary's identity changes
on every rebuild, so the grant is orphaned and macOS re-prompts each time. Sign the installed
binary with a **stable** self-signed identity instead; the header of
[`contrib/dev-deploy.sh`](../contrib/dev-deploy.sh) documents the one-time certificate setup and
the script then builds, signs, and installs to `~/.local/bin` in one step.

Both options above already do this: `contrib/build-gui.sh` signs the copy it bundles (so the copy
the app installs carries a stable identity), and `contrib/dev-deploy.sh` is the command-line
equivalent. Only a hand-rolled `cp` skips it.

## 2. Choose a summarizer

Munshi never calls a model API directly. Instead, every session is handed to an
**executable you choose**, which reads a JSON summary request (including the session's
normalized transcript) on stdin and prints one JSON object matching Munshi's summary schema.
See [`docs/summarizers.md`](summarizers.md) for the exact contract.

**Use one of the bundled wrappers** —
[`contrib/copilot-summarizer.sh`](../contrib/copilot-summarizer.sh) (Copilot CLI in
noninteractive `-s` mode) or [`contrib/claude-summarizer.sh`](../contrib/claude-summarizer.sh)
(`claude -p` with a haiku model). Both normalize the model's output into Munshi's schema
(stripping Markdown fences, backfilling empty list fields) and, critically, isolate the harness
home the summarizer runs under. Make your pick executable:

```bash
chmod +x /absolute/path/to/munshi/contrib/claude-summarizer.sh
```

Each wrapper defaults its binary to the Homebrew symlink and takes environment overrides rather
than edits: `COPILOT_BIN` for the copilot wrapper; `CLAUDE_BIN` and `CLAUDE_MODEL` for the
claude wrapper (set them at registration with `--summarizer-env KEY=VALUE`, or export them where
the hooks run). One extra step for the claude wrapper on a Keychain-backed macOS install: with a
relocated home, Claude Code reads credentials only from disk and does **not** fall back to the
Keychain, so authenticate the isolated home once —
`CLAUDE_CONFIG_DIR="$HOME/.claude-summarizer" claude /login` — and the wrapper picks it up from
then on (the script's header comment explains the full auth story and the alternatives).

Either wrapper works regardless of which harness (Copilot CLI or Claude Code) captured the
session — source capture and summarization are independent choices.

> **Why wrappers rather than the bare binaries?** Both `copilot -s` and `claude -p` are
> coding-agent CLIs that record a session of their own on every run. Left un-isolated, the
> sessions they create while summarizing are discovered as new work to summarize, and archiving
> feeds itself — this has happened in the field (issue #37). Pointing `--summarizer` directly at
> a bare `copilot` binary (`--summarizer /absolute/path/to/copilot --summarizer-arg=-s
> --summarizer-arg=--no-ask-user`) is deprecated and shown here only to make the contract clear:
> the summarizer is just an executable taking JSON on stdin. Details:
> [Summarizers that are themselves session-recording harnesses](summarizers.md#4-hazard-summarizers-that-are-themselves-session-recording-harnesses).

## 3. Register Munshi

Registration installs lifecycle hooks for your coding-agent CLI(s), writes Munshi's config, and
sets up the archive directory. It requires an explicit output directory, an explicit summarizer,
and an explicit acceptance of the transcript-processing disclosure:

```bash
munshi register \
  --accept-transcript-processing \
  --output-dir /absolute/path/to/munshi-summaries \
  --summarizer /absolute/path/to/munshi/contrib/copilot-summarizer.sh
```

(or `--summarizer /absolute/path/to/munshi/contrib/claude-summarizer.sh` — whichever wrapper you
picked in step 2.)

By default `register` targets every harness whose home directory it finds on disk. To be
explicit, pass `--harness copilot` and/or `--harness claude-code` (repeatable), or point directly
at a harness home with `--copilot-home`/`--claude-home`. Pass `--dry-run` first if you just want
to see the managed paths Munshi would write without touching anything (it still asks you to accept
the disclosure first — the prompt runs before the dry-run report, so `--dry-run` without
`--accept-transcript-processing` stops at the prompt).

Optionally, pass `--archive-git-history` to have Munshi commit each successful summary revision
into a dedicated Git repository inside your output directory.

A few more registration-time flags are worth knowing about up front:

- `--summarizer-env KEY=VALUE` (repeatable) sets environment variables on every summarizer
  invocation — this is how you pin `CLAUDE_BIN`/`CLAUDE_MODEL`/`COPILOT_BIN` for the wrappers,
  or select per-phase chunk/reduce models (`MUNSHI_CHUNK_MODEL`/`MUNSHI_REDUCE_MODEL`, see
  [`docs/summarizers.md`](summarizers.md)).
- `--summarizer-exhaust-home` plus `--summarizer-exhaust-retention-days` are
  **registration-only** knobs that let `munshi tick` prune the isolated `COPILOT_HOME` that
  `contrib/copilot-summarizer.sh` runs under, typically `~/.copilot-summarizer` (issue #60).
  Without them nothing is ever pruned, and that home grows without bound — measured at roughly **5.6 GB after a month** of normal use on one machine. See the
  [summarizer-exhaust retention section of the user guide](user-guide.md#summarizer-exhaust-retention).
- `--chunk-threshold-bytes` and `--chunk-size-bytes` control when and how marathon sessions are
  summarized in chunks instead of one shot (issue #48).
- Size and timeout caps (`--timeout-ms`, `--max-source-bytes`, `--max-input-bytes`, and friends)
  all have sensible defaults; see [`docs/configuration.md`](configuration.md) for the full list
  and how the size knobs relate.

If you omit `--accept-transcript-processing`, an interactive terminal prompts you to type the
exact text `I ACCEPT`; noninteractive registration without the flag fails outright.

### Read this before you accept

The disclosure you're accepting is real, so here it is plainly:

- Once registered, summarization is **on by default for every project** you work in with a
  registered harness — there's no per-project opt-in step.
- Munshi sends the **full session transcript** to your configured summarizer on every completed
  session. If your summarizer calls a hosted model (as both examples above do), that consumes
  API credits or usage against your account.
- v1 does **not** redact secrets or filter events out of the transcript before summarizing.
  Don't register a summarizer you don't trust with your raw session content.
- You can stop this for a single project at any time with `munshi project disable
  /absolute/path/to/project` (see [`docs/user-guide.md`](user-guide.md)).
- Cost is also bounded by default: each project gets 10 summarizer calls/hour, 50/day, with at
  most 2 sessions summarized concurrently across all projects. Adjust these at registration time
  with `--max-calls-per-hour`, `--max-calls-per-day`, and `--max-concurrency`.

Where things get written:

- **State** (`config.json`, `munshi.db`, locks): `$MUNSHI_HOME`, or `~/.munshi` by default.
- **Archives**: the `--output-dir` you chose.
- **Copilot hooks**: `~/.copilot/hooks/munshi.json` (or `$COPILOT_HOME/hooks/munshi.json`).
- **Claude Code hooks**: merged into `~/.claude/settings.json` (or
  `$CLAUDE_CONFIG_DIR/settings.json`) — Munshi only appends its own managed `Stop` and
  `SessionEnd` entries and leaves every other key, hook, and your file's formatting untouched.

## 4. Restart your coding-agent CLI

Both Copilot CLI and Claude Code read their hook configuration at session startup, so register
with **no active session running**, then start (or restart) the CLI before your next session so
it picks up Munshi's hooks.

## 5. Schedule maintenance

Hook events already run recovery sweeps and retries on a busy machine, but on an *idle* machine
parked retries, pending uploads, and failed deliveries only drain when something fires them. That
something is `munshi tick` (issue #55): one idempotent maintenance sweep that prints nothing when
there is nothing to do, made to be run forever by a platform timer. On macOS, install the bundled
15-minute launchd agent (edit the absolute paths in it first — launchd expands no `~`, so
replace every `YOUR_USERNAME` placeholder in the plist with your own account name):

```bash
cp contrib/launchd/com.munshi.tick.plist ~/Library/LaunchAgents/
launchctl bootstrap gui/$UID ~/Library/LaunchAgents/com.munshi.tick.plist
```

On Linux, a systemd user timer running `munshi tick` every 15 minutes is the equivalent. See
[the tick section of the user guide](user-guide.md#munshi-tick) for what a tick does and doesn't
touch.

## 6. Verify it's working

Check that registration is healthy:

```bash
munshi doctor
munshi configuration-check
munshi status
```

`doctor` diagnoses registration, dependencies, and runtime readiness; `configuration-check`
validates the stored configuration contracts; `status` shows the operational counters you'll
watch day to day. Don't expect a wall of green — `doctor` legitimately warns on real conditions
(for example a summarizer-exhaust home past 1 GiB), and a warning is information, not failure.
Then run one short real session in some project directory and exit
cleanly — for example:

```bash
cd /absolute/path/to/some/project
claude -p "hello"
```

(or a normal interactive Copilot CLI session that you exit normally). Give it a moment, then look
for it:

```bash
munshi sessions
munshi show <session-id> --source claude-code
```

You should also find the archive on disk. Claude Code sessions land at:

```text
<output-dir>/<project-component>/claude-code/<session-id>.md
```

Copilot sessions land flat under the project component:

```text
<output-dir>/<project-component>/<session-id>.md
```

If you installed the desktop app in step 1, everything above has a visual equivalent: open it and
the same session appears in the list, with its summary, its state, and buttons to re-summarize or
retry. It reads the very same commands — nothing is displayed there that `munshi status`,
`munshi sessions` and `munshi show` will not tell you. Use whichever you prefer; the app is a
window onto the CLI, not a second implementation.

## Archiving one session manually (no registration needed)

If you just want to try Munshi on one existing transcript without installing hooks, `munshi
archive` runs the same summarize-and-write path standalone:

```bash
munshi archive --source claude-code \
  --events "$HOME/.claude/projects/<munged-cwd>/<uuid>.jsonl" \
  --project-dir /absolute/path/to/project \
  --output-dir /absolute/path/to/munshi-summaries \
  --summarizer /absolute/path/to/summarizer
```

`--source` defaults to `copilot`; `--source claude-code` and `--source codex` archive the other
harnesses' transcripts — Codex has no hook support at all, so pointing `--events` at a rollout
file is the *only* way its sessions get archived. Full detail in
[`docs/manual-archive.md`](manual-archive.md).

## Uninstalling

```bash
munshi unregister
```

This removes only Munshi's own hook entries and config — it never touches other hooks in your
`settings.json`, and it never deletes archives or state. Delete `$MUNSHI_HOME` (`~/.munshi` by
default) yourself if you also want the operational state gone.

## Where to go next

- The three post-register opt-in remote sinks, all disabled until you enable them:
  `munshi summary-delivery` (summaries into a Notesmith vault), `munshi archive-upload`
  (full session snapshots to a Patwari archive server), and `munshi memory-sync` (harness
  auto-memory mirrored into Notesmith, issue #59). The
  [user guide](user-guide.md) covers the first two in brief;
  [`docs/configuration.md`](configuration.md) documents all three sinks' settings.
- [`docs/summarizers.md`](summarizers.md) — the summarizer contract, if you want to write your own.
- [`docs/user-guide.md`](user-guide.md) — day-to-day operation: disabling projects, retries, budgets.
- [`docs/configuration.md`](configuration.md) — every `config.json` setting in one place.
- [`docs/manual-archive.md`](manual-archive.md) — the standalone `munshi archive` path in full.
- [`docs/dashboard.md`](dashboard.md) — the backlog dashboard addon over Munshi's JSON contracts.
- [`docs/troubleshooting.md`](troubleshooting.md) — what to check when a session doesn't archive.
- [`docs/automatic-archive.md`](automatic-archive.md) — full detail on hooks, recovery, and state.
