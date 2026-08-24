# Defer the analysis client until a first consumer exists

A fourth suite component was designed and deliberately not built: a read-side analysis client
(working name Qanungo, the officer who audited patwaris' records) with an incremental sync mirror
of the archive, a content-addressed local cache, a rebuildable SQLite store of normalized events,
and application commands such as a prompt-corpus exporter or a session chronicle. All of it is
deferred until a first real consumer exists. Patwari already provides the storage-and-retrieval
half of any such pipeline — cursored incremental snapshot discovery, hash-verified downloads,
capture provenance, hash-addressed lookup — and the archive is durable and immutable, so every
derived structure remains re-derivable later at no loss. Building the mirror and event store now
would be speculation shaped by no consumer.

The plumbing boundary is therefore [ADR 0011](0011-interpret-transcripts-at-read-time-through-a-shared-streaming-crate.md)'s
`munshi-transcript` crate plus the retrieval Patwari already serves. The one time-sensitive proof
happens now: `munshi verify-archive-parse` walks the archive, downloads and verifies artifacts,
stream-parses every transcript, and reports per-session accounting (records seen, share typed,
`Unknown` kinds, record errors), because parsing the real archive is how a capture gap is
discovered while original local session files still exist. It remains a manual check rerun after
format bumps, not a scheduled job — nothing yet depends on freshness.

When a consumer does exist, the client gets its own repository, matching the suite's
independently-deployable precedent, and consumes `munshi-transcript` as a git dependency pinned to
a tag. Its derived store stays a disposable private implementation detail — rebuild means delete
and resync — not a stable contract; Patwari remains the only stable interface. Analysis features
land there as commands over the event store, and curated prose output goes to Notesmith,
preserving the existing boundary in which Patwari never interprets, ranks, or searches content.

The working name held: that consumer now exists as the repository `surdy/qanungo`, and its
metrics pull typed fields into `munshi-transcript` one at a time (issue #77): a shell `command` for
the churn metric, then per-message model and token usage for the cost lane. Each promotion re-reads
the same immutable record rather than rewriting one, which is what lets typed signals keep growing
under ADR 0011's read-time rule without touching anything already archived.
