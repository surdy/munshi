# Elide with claim tickets, retrieve on demand

Oversized normalized event content is extracted, not truncated: the full content becomes its own
content-addressed snapshot artifact, and the summarizer input carries a claim ticket in its place —
a marker with the original content's sha256, size, and a short label. The rendered Markdown
frontmatter indexes the snapshot's artifacts (transcript and extracted-output hashes, capture ID),
so every summary is losslessly expandable: nothing the summary elides is ever unreachable, it is one
hash lookup away in Patwari. This adapts the compress-cache-retrieve pattern from LLM context
proxies, with the durable archive standing in for their ephemeral cache.

Ticket hashes are computed locally before any upload, so summaries render and deliver to Notesmith
without waiting on Patwari; content addressing guarantees a ticket resolves once its snapshot
lands. `munshi retrieve <sha256> [--query]` redeems a ticket: it resolves the hash through Patwari's
artifact lookup, downloads the stored bytes, verifies both stored and original hashes, decompresses
locally, and optionally searches within the content. Search stays client-side; Patwari remains an
uninterpreting archive.

The summarizer contract stays one-shot: the summarizer receives tickets but cannot redeem them
mid-run. An iterative contract letting a summarizer request elided content while summarizing is a
known future direction that tickets make possible without further archive changes.
