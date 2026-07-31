# Munshi backlog dashboard (contrib)

A local, read-only, single-page dashboard over munshi's operational state: session-state
distribution, error categories, upload/delivery ledgers, and per-session drill-down with
archive titles. Built ad hoc during the 2026-07 backlog-archival drive (session
`88733b7c`, served on `127.0.0.1:8877`), where it earned its keep debugging the drive;
rescued here verbatim from that session's scratchpad. Spike quality, personal tool — not
part of munshi proper.

Run:

```sh
python3 server.py     # serves http://127.0.0.1:8877, /api/data refreshes ~30s
```

Data sources (all read-only):

- `munshi status --json`, `munshi archive-upload status --json`,
  `munshi summary-delivery status --json` — the ADR 0007 contract surface.
- A **copy** of `~/.munshi/munshi.db` (never the live DB) for queries the JSON contracts
  do not yet expose. This is a deliberate ADR 0007 deviation acceptable in a personal
  spike: the schema is rebuildable internal state, not a contract, so expect this to
  break across munshi versions. Productizing this dashboard means widening the `--json`
  contracts until the DB copy is unnecessary.
- `# ` H1 titles from `~/munshi-summaries` Markdown.
- The drive's `backlog-driver.log` time series (path is hardcoded to the original
  session scratchpad and no longer exists; the panel degrades to empty).

Paths at the top of `server.py` are hardcoded for this machine.
