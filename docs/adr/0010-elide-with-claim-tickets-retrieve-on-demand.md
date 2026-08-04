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
mid-run over the subprocess protocol. An iterative stdin/stdout contract remains a known future
direction that tickets make possible without further archive changes.

**Update (2026-08-03, issue #25):** mid-run redemption exists without changing the subprocess
contract. `munshi retrieve <sha256> --local --session <id>` redeems a ticket from the session's
on-disk transcript — necessary because upload runs strictly downstream of summarization, so at
summarize time Patwari does not yet hold the current revision's artifacts — and an agentic
summarizer with shell access (see `contrib/claude-summarizer.sh`, opt-in
`MUNSHI_TICKET_REDEMPTION=1`) redeems tickets itself while summarizing. Only hash-verified bytes
are ever emitted, so a live transcript mutating under the read cannot yield wrong content. The
iterative protocol stays deferred until agentic redemption's lack of Munshi-side accounting
demonstrably hurts.
