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

The full walkthrough — including the disclosure you're accepting and how to pick a summarizer —
is in [getting started](docs/getting-started.md).

## The wider ledger

Munshi is the local scribe in a small family of tools:

- **[Patwari](https://github.com/surdy/patwari)** — a self-hosted archive server for *complete*
  session captures: verified, immutable, content-addressed. Munshi will submit full transcripts
  there for permanent keeping.
- **Notesmith** — an optional notes sink; Munshi can deliver each summary into a vault alongside
  your own notes (disabled by default, opt-in via `munshi delivery`).
- **Madari** — a session-discovery GUI that reads Munshi's JSON status contracts to show archive
  state next to each discovered session.

Each integration is optional; Munshi is fully useful as a standalone CLI.

## Documentation

| Guide | What it covers |
| --- | --- |
| [Getting started](docs/getting-started.md) | Build, register, and see your first archived session |
| [Summarizers](docs/summarizers.md) | The summarizer contract, Copilot and Claude Code examples, writing your own |
| [Using Munshi day to day](docs/user-guide.md) | Status, sessions, retries, budgets, project policy, delivery |
| [Troubleshooting](docs/troubleshooting.md) | Why a session didn't archive, and how to find out |

Deeper reference: the [design and product specification](docs/design.md) (the long-form record
this README used to be), [automatic archival](docs/automatic-archive.md),
[manual archival](docs/manual-archive.md), [harness adapters](docs/harness-adapters.md),
[ADRs](docs/adr/), and the phase-0 probe findings for
[Copilot CLI 1.0.70](docs/phase-0-findings.md) and
[Claude Code 2.1.205](docs/phase-0-claude-code-findings.md).

## Status

Actively developed. Automatic capture, summarization, resumed-session revisions,
interrupted-session recovery, per-project policy and budgets, operational CLI contracts
(`status`, `sessions`, `show`, `retry`, `doctor`, …), optional archive Git history, and opt-in
Notesmith delivery are all implemented and tested. Full-transcript backup to Patwari is the next
major slice. License: MIT OR Apache-2.0.
