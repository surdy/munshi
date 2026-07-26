# Interpret transcripts at read time through a shared streaming crate

What an archived transcript means is decided when it is read, not when it is captured: Patwari
snapshots keep only source-of-truth bytes, and interpretation lives in a workspace crate,
`munshi-transcript`, extracted from the source adapters. Derived data is never baked into an
immutable snapshot, because extraction logic improves and a re-derivable read-time pass can always
be rerun over the archive, while a capture-time artifact freezes today's extraction bugs forever.
Parsing is keyed by the `source_agent` and `artifact_set_version` that capture provenance already
carries, so format knowledge stays in exactly one place as agents and artifact sets evolve.

The crate exposes a streaming, lossless contract: it takes any buffered reader and yields one item
per transcript record — a typed event carrying full content (complete user and assistant text, tool
name and payload), an `Unknown` variant carrying the raw record, or a per-record error. Every line
of a transcript is accounted for exactly once; a malformed record becomes an inspectable item, not
an aborted parse. Streaming composes with the retrieval path (HTTP body, Zstandard decode, line
reader, events) under bounded memory, and a collect-to-`Vec` convenience is trivial on top while
the reverse retrofit is not. A lossy stream would hide precisely the records that reveal
interpretation gaps.

The capture path is rebuilt on the crate rather than left alongside it: `NormalizedSession`'s
counts become a fold over the event stream and the superseded per-adapter counting code is deleted.
Two implementations of what a transcript means would drift — the capture side maintained, the
read-time side silently rotting — and rebuilding means every archive run of every session exercises
the one parser, so format changes surface at capture time. Archive-worthiness rules are unchanged;
they are simply fed by the stream. Fixture tests gate equivalence: every sanitized adapter fixture
must stream-parse with no `Unknown` records and no record errors, and old counts must equal the
fold.
