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

## Where this fits

Munshi is the **capture** side of a three-tool suite: it runs on each of your machines, records
what happened, and ships full session snapshots onward. *Munshi writes the record; Patwari keeps
the archive; Qanungo audits it.*

**Start at [daftar](https://github.com/surdy/daftar)** — the suite's front door, with the pipeline
diagram, the install order, and a fifteen-minute path from nothing to a first report.

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

## Trust and authentication

Be aware before you point Munshi at a server: the full-snapshot upload to Patwari carries **no
credential at all**, because Patwari has no authentication — this suite was built as one person's
private tooling for their own LAN and tailnet, where every client that can reach the archive is
already trusted. So point `munshi archive-upload configure --endpoint` only at a Patwari on a
private network you control (localhost, a LAN address, a tailnet name, or an `https://` name
published behind a TLS-terminating reverse proxy that only resolves inside it) — never at anything
reachable from the open internet, and add your own authenticating proxy in front of it if you need
one. Know what identity travels with an upload, too: the capture machine label — the sanitized
hostname unless you set one — is recorded on every uploaded snapshot and on this machine's client
record in the archive, visible to everyone who can read it; override it with `munshi archive-upload
configure --machine-label <label>` (or `munshi memory-sync configure --machine <label>`, the same
stored label). Notesmith summary delivery is different: `munshi summary-delivery configure` can be given a
credential *source* — an environment variable or an OS keychain entry — and Munshi reads the token
from it at delivery time and sends `Authorization: Bearer …`, never storing the secret itself
(the credential is optional, for an unauthenticated local daemon). None of this is a judgement
about what you should run; anyone is welcome to take this suite and adapt it, and these are the
assumptions you would be adapting.

## Quick start

```bash
cargo build --release
mkdir -p ~/.local/bin && cp target/release/munshi ~/.local/bin/munshi
munshi register --accept-transcript-processing \
  --output-dir ~/munshi-archives \
  --summarizer /absolute/path/to/munshi/contrib/claude-summarizer.sh
munshi doctor
```

Registration records the **absolute path** of the binary in your hook files, so install it
somewhere stable — `~/.local/bin` — before registering, rather than registering out of
`target/release`. Then finish any coding session and look in `~/munshi-archives`:

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
- **[Qanungo](https://github.com/surdy/qanungo)** — the read side: it mirrors the Patwari archive,
  re-reads the transcripts Munshi captured, and reports on how the sessions actually went —
  per-device scope, cost, and coaching findings. Munshi and Patwari never interpret content;
  Qanungo is where interpretation lives ([ADR 0012](docs/adr/0012-defer-the-analysis-client-until-a-first-consumer-exists.md)).
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
| [Using Munshi day to day](docs/user-guide.md) | Status, sessions, retries, budgets, project policy, delivery, archive upload, restore |
| [Configuration reference](docs/configuration.md) | Every `config.json` setting in one place |
| [Shipping to Patwari](docs/shipping-to-patwari.md) | The capture side of the archive seam: what a snapshot carries, configure/enable, how uploads drain, device identity, second-machine checklist |
| [Manual archival](docs/manual-archive.md) | Archiving a single transcript standalone, no hooks required |
| [Desktop addon](docs/gui.md) | The optional native app, and installing the CLI it ships with |
| [Backlog dashboard addon](docs/dashboard.md) | Visualizing the operational JSON contracts (issue #56) |
| [Troubleshooting](docs/troubleshooting.md) | Why a session didn't archive, and how to find out |
| [Glossary (`CONTEXT.md`)](CONTEXT.md) | The vocabulary this project uses, and the words it avoids |
| [`contrib/skills/session-recall`](contrib/skills/session-recall/) | A coding-agent skill for searching your own past sessions (see [Skills](#skills)) |
| [`contrib/summary-retention.py`](contrib/summary-retention.py) | A local, read-only measurement of how much of a session survives into its summary |

Deeper reference: the [design and product specification](docs/design.md) (the long-form record
this README used to be), [automatic archival](docs/automatic-archive.md),
[harness adapters](docs/harness-adapters.md),
[all fourteen ADRs](docs/adr/) — start with
[0001 session identity](docs/adr/0001-identify-logical-sessions-by-source-id.md),
[0002 Markdown as the durable record](docs/adr/0002-make-markdown-the-durable-record.md), and
[0003 separate local and Notesmith history](docs/adr/0003-keep-local-and-notesmith-history-separate.md) —
and the phase-0 probe findings for
[Copilot CLI 1.0.70](docs/phase-0-findings.md) and
[Claude Code 2.1.205](docs/phase-0-claude-code-findings.md).

## Skills

Munshi ships one coding-agent skill under [`contrib/skills/`](contrib/skills/). Drop it into your
agent's skills directory and it becomes the friendliest way to use the archive: you ask a
question in plain language instead of remembering a command.

| Skill | Triggers on | What it does | Needs | Writes |
| --- | --- | --- | --- | --- |
| [`session-recall`](contrib/skills/session-recall/) | "what happened in that session", "how did we do X before", "why did we decide Y", or a bare session ID | A funnel: scored full-text search over your Notesmith summary notes first (cheap), escalating to the verbatim transcript from the Patwari archive by content hash only when a summary can't answer | `NOTESMITH_URL` and `PATWARI_URL` set to your own endpoints | **Nothing** — every call is read-only |

## Status

*Last reviewed 2026-09-04.*

**Shipped.** Automatic capture, summarization, resumed-session revisions, interrupted-session
recovery, per-project policy and budgets, chunked marathon summarization (issue #48), optional
archive Git history, and all three opt-in remote sinks: Notesmith summary delivery (including
`summary-delivery history`), full-snapshot archive upload to Patwari with claim-ticket redemption
per [ADR 0009](docs/adr/0009-archive-full-snapshots-to-patwari.md) and
[ADR 0010](docs/adr/0010-elide-with-claim-tickets-retrieve-on-demand.md), and harness auto-memory
mirroring (`munshi memory-sync`, issue #59). Local-record recovery from the archive (`munshi
restore`, issue #70) with Claude Code session resumption (`--resume`, issue #71). Published
`https://` endpoints are spoken natively (issue #35,
[ADR 0013](docs/adr/0013-speak-tls-to-published-endpoints-through-rustls-and-the-system-trust-store.md));
an idle machine drains its backlog through `munshi tick` on a platform timer (issue #55); the
desktop app ([ADR 0014](docs/adr/0014-ship-the-desktop-addon-over-the-same-cli-json-boundary.md))
and the backlog dashboard addon (issue #56) both read the same JSON contracts. Capture provenance
travels with every upload (`utc_offset`, `hostname`, and the `CLAUDE.md`/`AGENTS.md` digests of
issue #77), which is what lets Qanungo scope by device.

**Deferred.** Delta-upload optimization (issue #24) — the transfer accounting that would justify it
is in `archive-upload status` and does not yet. Codex CLI stays manual-archive only: no lifecycle
hooks, and `restore --resume` is refused for it by design.

The full command surface, grouped:

| Group | Commands |
| --- | --- |
| Setup | `register`, `unregister`, `project`, `doctor`, `configuration-check` |
| Sinks (opt-in, each disabled by default) | `summary-delivery`, `archive-upload`, `memory-sync` |
| Read | `status`, `sessions`, `show`, `attempts`, `diagnostics` |
| Maintenance | `tick`, `retry`, `retry-all`, `archive` (one manual session), `purge-mismatched`, `settle-lost` |
| Recovery | `retrieve`, `restore`, `verify-archive-parse` |

Every read command takes `--json` and emits a stable machine-readable contract. `munshi hook` and
its `recover` subcommand are hidden from `--help`: they are the recovery path the hooks and
`munshi tick` run for you, documented in
[troubleshooting](docs/troubleshooting.md) for when you need to run one by hand.

## The name

A *munshi* (Hindi मुंशी, Urdu منشی, from Persian) was a clerk, scribe, or secretary — the person
in the room whose job was to keep the written record while everyone else did the talking. That is
exactly what this tool is: the scribe for your coding-agent sessions.

The companion projects are named in the same spirit: [Patwari](https://github.com/surdy/patwari)
(पटवारी, پٹواری), after the village record-keeper who maintained the permanent land ledger, and
[Qanungo](https://github.com/surdy/qanungo) (क़ानूनगो, قانونگو), the officer above him who audited
those records — three offices of one record room, which is what
[daftar](https://github.com/surdy/daftar) (दफ़्तर, دفتر) is named for.
**Munshi writes the record; Patwari keeps the archive; Qanungo audits it.**

## License

Dual-licensed under either [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option.
