<p align="center">
  <img src="brand/header.svg" alt="munshi — a quiet scribe for your AI coding sessions" width="720">
</p>

Munshi quietly archives your AI coding sessions. When you finish a session in GitHub Copilot CLI
or Claude Code, Munshi captures it through the harness's own lifecycle hooks, has a model
summarize what happened, and writes one durable Markdown record per session — what you set out to
do, what got done, what was decided, which files changed, and what's still open. Resume the
session later and the same record is revised, not duplicated.

Everything is local-first: no daemon, no server, no cloud account. Your archive is a folder of
readable Markdown files (optionally with its own Git history), owned by you.

## The name

A *munshi* (Hindi मुंशी, Urdu منشی, from Persian) was a clerk, scribe, or secretary — the person
in the room whose job was to keep the written record while everyone else did the talking. That is
exactly what this tool is: the scribe for your coding-agent sessions.

The companion project is named in the same spirit: [Patwari](https://github.com/surdy/patwari)
(पटवारी, پٹواری), after the village record-keeper who maintained the permanent land ledger.
**Munshi writes the record; Patwari keeps the archive.**

## What it does

- **Captures automatically.** Registering installs lifecycle hooks for Copilot CLI and/or Claude
  Code. Sessions archive themselves when they end; force-killed sessions are recovered by a
  background sweep. No polling, no terminal scraping.
- **Summarizes with a model you choose.** Munshi never calls a model API itself — it pipes the
  session to a summarizer executable you configure (Copilot CLI, a bundled Claude Code wrapper,
  or your own script).
- **Writes durable Markdown.** One file per logical session with structured YAML front matter.
  Markdown is the source of truth; Munshi's SQLite bookkeeping can always be rebuilt from it.
- **Defers, never drops.** Per-project hourly/daily budgets and a concurrency cap bound
  summarizer cost; anything over budget waits its turn instead of being lost.
- **Stays in its lane.** Explicit disclosure before anything is processed, per-project
  disable, content-free diagnostics, and an uninstall that removes exactly what it installed.

Supported today: **GitHub Copilot CLI** and **Claude Code** capture automatically; OpenAI Codex
CLI sessions can be archived manually. macOS and Linux.

## Quick taste

```bash
cargo build --release
./target/release/munshi register --accept-transcript-processing \
  --output-dir ~/munshi-archives \
  --summarizer /absolute/path/to/munshi/contrib/claude-summarizer.sh
```

Then finish any coding session and look in `~/munshi-archives`:

```text
my-project-6c79ec1dbed7/claude-code/e0820fcc-….md   # title, goal, work completed,
                                                     # decisions, files changed, open items
```

Prefer a window to a terminal? Build the optional desktop app instead — it bundles this same
`munshi` binary and installs it for you:

```bash
./contrib/build-gui.sh    # then open gui/src-tauri/target/release/bundle/…
```

Registration still happens once in a terminal (it discloses what Munshi will process, and you have
to accept that), but from then on the app shows the backlog, every session and its summary, and
the retry actions. It reads Munshi's published JSON contracts and nothing else, so the two views
never disagree — and Munshi works exactly the same with the app absent. See
[the desktop addon guide](docs/gui.md).

The full walkthrough — including the disclosure you're accepting and how to pick a summarizer —
is in [getting started](docs/getting-started.md).

## The wider ledger

Munshi is the local scribe in a small family of tools:

- **[Patwari](https://github.com/surdy/patwari)** — a self-hosted archive server for *complete*
  session captures: verified, immutable, content-addressed. Munshi submits full session
  snapshots there for permanent keeping: the verbatim transcript, each summary revision, and
  oversized tool outputs preserved as individually retrievable artifacts.
- **Notesmith** — an optional notes sink; Munshi delivers each summary into a vault alongside
  your own notes (disabled by default, opt-in via `munshi summary-delivery`).
- **Madari** — a session-discovery GUI that reads Munshi's JSON status contracts to show archive
  state next to each discovered session.

Munshi's own optional surfaces — the [desktop app](docs/gui.md) and the
[backlog dashboard](docs/dashboard.md) — sit on the same versioned JSON contracts these
integrations use. Both are addons; the command line stays the primary way to run Munshi.

Remote flow happens through exactly three opt-in sinks, each disabled by default and enabled
separately: `munshi summary-delivery` (summaries to Notesmith), `munshi archive-upload`
(full snapshots to Patwari), and `munshi memory-sync` (harness auto-memory mirrored into
Notesmith, issue #59). Each integration is optional; Munshi is fully useful as a standalone CLI.

## Documentation

| Guide | What it covers |
| --- | --- |
| [Getting started](docs/getting-started.md) | Build, register, and see your first archived session |
| [Summarizers](docs/summarizers.md) | The summarizer contract, Copilot and Claude Code examples, writing your own |
| [Using Munshi day to day](docs/user-guide.md) | Status, sessions, retries, budgets, project policy, delivery |
| [Configuration reference](docs/configuration.md) | Every `config.json` setting in one place |
| [Manual archival](docs/manual-archive.md) | Archiving a single transcript standalone, no hooks required |
| [Desktop addon](docs/gui.md) | The optional native app, and installing the CLI it ships with |
| [Backlog dashboard addon](docs/dashboard.md) | Visualizing the operational JSON contracts (issue #56) |
| [Troubleshooting](docs/troubleshooting.md) | Why a session didn't archive, and how to find out |

Deeper reference: the [design and product specification](docs/design.md) (the long-form record
this README used to be), [automatic archival](docs/automatic-archive.md),
[harness adapters](docs/harness-adapters.md),
[ADRs](docs/adr/), and the phase-0 probe findings for
[Copilot CLI 1.0.70](docs/phase-0-findings.md) and
[Claude Code 2.1.205](docs/phase-0-claude-code-findings.md).

## Status

Actively developed. Automatic capture, summarization, resumed-session revisions,
interrupted-session recovery, per-project policy and budgets, chunked marathon summarization
(issue #48), operational CLI contracts (`status`, `sessions`, `show`, `retry`, `doctor`, …),
optional archive Git history, and all three opt-in remote sinks are implemented and tested:
Notesmith summary delivery, full-snapshot archive upload to Patwari — `archive-upload
configure`/`enable`/`backfill`/`retry`, claim-ticket redemption via `munshi retrieve`, local-record
recovery from the archive via `munshi restore` (issue #70) with Claude Code
session resumption via `munshi restore --resume` (issue #71), and the
`verify-archive-parse` acceptance walk, per
[ADR 0009](docs/adr/0009-archive-full-snapshots-to-patwari.md) and
[ADR 0010](docs/adr/0010-elide-with-claim-tickets-retrieve-on-demand.md) — and harness
auto-memory mirroring (`munshi memory-sync`, issue #59). Published `https://` endpoints are
spoken natively (issue #35,
[ADR 0013](docs/adr/0013-speak-tls-to-published-endpoints-through-rustls-and-the-system-trust-store.md)),
an idle machine drains its backlog through `munshi tick` on a platform timer (issue #55), and a
backlog dashboard addon visualizes the operational contracts (issue #56; see
[docs/dashboard.md](docs/dashboard.md)). License: MIT OR Apache-2.0.
