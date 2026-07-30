---
name: session-recall
description: Recall the user's past coding-agent sessions. Search session summaries in Notesmith, then escalate to the full verbatim transcript from the Patwari archive when the summary isn't enough. Use when asked what happened in a past session, how/why something was done before, to find prior art or decisions from earlier work, or when given a session ID to dig into.
---

# Recalling past coding-agent sessions

Every coding-agent session (Claude Code, Copilot CLI) on this machine is archived by Munshi:
a summary note in Notesmith, and the full verbatim transcript in the Patwari archive. This
skill is a funnel — **always start with summaries (cheap), escalate to transcripts (heavy)
only when the summary can't answer.**

Both services are LAN/tailnet-local: requests cost nothing and content never leaves the
user's network. All access below is read-only.

## Stage 1 — search the summaries (always start here)

Scored full-text search over all session summaries:

```sh
curl -s "https://notesmith.clusterfault.com/api/v/sessions/search?q=<terms>" | python3 -m json.tool
```

Each hit has `path`, `title`, `score`, and a `snippet`. Read a full note:

```sh
curl -s "https://notesmith.clusterfault.com/api/v/sessions/notes/<path>"
```

The JSON carries `frontmatter` (`munshi_session` = session ID, `munshi_source` =
`copilot`/`claude-code`, `munshi_revision`) and `body` (the summary: goal, work completed,
decisions, files changed, open items). Notes are organized `<project-component>/<source>-<session-id>.md`;
list everything with `.../api/v/sessions/notes`.

Most questions end here. Summaries are deliberately curated; only escalate when you need
verbatim detail (exact commands, exact errors, exact code, elided tool output).

## Stage 2 — locate the transcript's content hash

The local archive Markdown for a session carries the claim hashes in its frontmatter:

```sh
sed -n '1,40p' ~/munshi-summaries/<project-component>/[claude-code/|codex/]<session-id>.md
```

(Copilot sessions sit flat in the component dir; Claude Code/Codex nest one level.) Take
`source_hash` (the transcript's sha256) — or an `extracted_outputs` entry's hash to redeem a
claim ticket for one elided oversized output instead of the whole transcript.

## Stage 3 — fetch the verbatim content from Patwari

Hash-addressed lookup, then download and decompress:

```sh
HASH=<64-hex, strip any "sha256:" prefix>
curl -s "https://patwari.clusterfault.com/api/v1/artifacts?original_sha256=$HASH" \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['items'][0]['content_url'])"
curl -s "https://patwari.clusterfault.com<content_url>" -o /tmp/artifact.bin
zstd -d -q -f /tmp/artifact.bin -o /tmp/artifact.out 2>/dev/null || mv /tmp/artifact.bin /tmp/artifact.out
```

(The response's `X-Patwari-Compression` header says `zstd` or `identity`; the fallback above
covers both.) One hash can resolve to multiple artifacts across snapshot revisions — items
are newest-first; the first is almost always right.

**Transcripts are large JSONL (up to hundreds of MB).** Never read one whole. Both formats
have one JSON record per line with a top-level `type`; user/assistant content lives in
`data.content` (Copilot) or `message.content` (Claude Code). Grep for what you need:

```sh
grep -c '' /tmp/artifact.out                       # size check first
grep -n '"user.message"' /tmp/artifact.out | head  # Copilot user prompts
grep -n '"type":"user"' /tmp/artifact.out | head   # Claude Code user records
```

## Direct entry with a known session ID

Skip stage 1: `munshi show <session-id>` prints the summary and the archive path (use
`--source claude-code|copilot` if ambiguous), then continue from stage 2. Browse the
archive's own metadata at `https://patwari.clusterfault.com/api/v1/sessions` (cursor-paginated).

## Caveats

- The current live session is archived with a lag (revisions land when it goes quiet) —
  recent turns may be missing from both summary and archive.
- A summary tagged `munshi-placeholder-summary` means summarization is still owed; go
  straight to the transcript.
- Never send transcript or summary content to external services; it is the user's private
  session history.
