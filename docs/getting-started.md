# Getting started with Munshi

Munshi is a local-first session archivist for coding-agent CLIs. When you finish a session in
GitHub Copilot CLI or Claude Code, Munshi shells out to a CLI you configure to produce a
structured summary, then writes a durable local Markdown archive of what happened — no daemon,
no server, no cloud storage. This guide walks you from a fresh clone to automatic archiving of
your everyday sessions.

## 1. Build Munshi

Munshi ships as a Rust workspace with no packaged releases yet, so you build it yourself:

```bash
cd /absolute/path/to/munshi
cargo build --release
```

The binary lands at `target/release/munshi`. Registration (below) records the **absolute path**
of this binary inside your hook files, so put it somewhere stable before registering — for
example copy it onto your `PATH`:

```bash
cp target/release/munshi /usr/local/bin/munshi
```

If you ever move or rebuild the binary at a new path, re-run `munshi register` afterward so the
hooks point at the right place.

## 2. Choose a summarizer

Munshi never calls a model API directly. Instead, every session is handed to an
**executable you choose**, which reads a JSON summary request (including the session's
normalized transcript) on stdin and prints one JSON object matching Munshi's summary schema.
See [`docs/summarizers.md`](summarizers.md) for the exact contract. Two ready-made options:

**Use Copilot CLI itself as the summarizer**, in noninteractive mode:

```bash
--summarizer /absolute/path/to/copilot \
--summarizer-arg=-s \
--summarizer-arg=--no-ask-user
```

**Use the bundled Claude-backed wrapper**, [`contrib/claude-summarizer.sh`](../contrib/claude-summarizer.sh).
It calls `claude -p` with a haiku model and normalizes the output. Before using it:

```bash
chmod +x /absolute/path/to/munshi/contrib/claude-summarizer.sh
```

...and edit the hardcoded `claude` binary path at the top of the script to match your install
(`which claude`, or check your Claude Code app's Caskroom/bin location).

Either summarizer works regardless of which harness (Copilot CLI or Claude Code) captured the
session — source capture and summarization are independent choices.

> **Both options are coding-agent CLIs that record a session of their own on every run.** Left
> un-isolated, the sessions they create while summarizing are discovered as new work to
> summarize, and archiving feeds itself — this has happened in the field (issue #37). Prefer the
> bundled wrappers ([`contrib/copilot-summarizer.sh`](../contrib/copilot-summarizer.sh),
> [`contrib/claude-summarizer.sh`](../contrib/claude-summarizer.sh)), which isolate their harness
> homes, over pointing `--summarizer` at a bare `copilot` or `claude` binary; the plain Copilot
> invocation above is shown for contract clarity, not as the recommended setup. Details:
> [Summarizers that are themselves session-recording harnesses](summarizers.md#4-hazard-summarizers-that-are-themselves-session-recording-harnesses).

## 3. Register Munshi

Registration installs lifecycle hooks for your coding-agent CLI(s), writes Munshi's config, and
sets up the archive directory. It requires an explicit output directory, an explicit summarizer,
and an explicit acceptance of the transcript-processing disclosure:

```bash
munshi register \
  --accept-transcript-processing \
  --output-dir /absolute/path/to/munshi-summaries \
  --summarizer /absolute/path/to/copilot \
  --summarizer-arg=-s \
  --summarizer-arg=--no-ask-user
```

By default `register` targets every harness whose home directory it finds on disk. To be
explicit, pass `--harness copilot` and/or `--harness claude-code` (repeatable), or point directly
at a harness home with `--copilot-home`/`--claude-home`. Pass `--dry-run` first if you just want
to see the managed paths Munshi would write without touching anything.

Optionally, pass `--archive-git-history` to have Munshi commit each successful summary revision
into a dedicated Git repository inside your output directory.

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

## 5. Verify it's working

Check that registration is healthy:

```bash
munshi doctor
```

All checks should report ok. Then run one short real session in some project directory and exit
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

## Uninstalling

```bash
munshi unregister
```

This removes only Munshi's own hook entries and config — it never touches other hooks in your
`settings.json`, and it never deletes archives or state. Delete `$MUNSHI_HOME` (`~/.munshi` by
default) yourself if you also want the operational state gone.

## Where to go next

- [`docs/summarizers.md`](summarizers.md) — the summarizer contract, if you want to write your own.
- [`docs/user-guide.md`](user-guide.md) — day-to-day operation: disabling projects, retries, budgets.
- [`docs/configuration.md`](configuration.md) — every `config.json` setting in one place.
- [`docs/troubleshooting.md`](troubleshooting.md) — what to check when a session doesn't archive.
- [`docs/automatic-archive.md`](automatic-archive.md) — full detail on hooks, recovery, and state.
