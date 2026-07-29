#!/bin/sh
# Munshi summarizer wrapper around Claude Code (`claude -p`).
#
# Bare `claude -p` is not usable as a Munshi summarizer directly: it tends to wrap its JSON
# answer in a Markdown code fence, and for trivial sessions it returns empty arrays for list
# fields, both of which fail Munshi's StructuredSummary validation (docs/summarizers.md).
# This wrapper strips any fence and backfills empty lists with ["none"].
#
# Phase-aware model selection (summarizer contract v2, issue #48): Munshi exports
# MUNSHI_SUMMARIZER_PHASE=complete|chunk|reduce on every invocation. The optional
# MUNSHI_CHUNK_MODEL and MUNSHI_REDUCE_MODEL environment variables select a different Claude
# model for the chunk / reduce invocations of a chunked marathon session; when they are unset
# (or the phase variable is absent, under a pre-v2 Munshi) every invocation uses CLAUDE_MODEL
# exactly as before.
#
# Adjust these two defaults for your machine (or override them via environment), then pass
# this script's absolute path to `munshi register --summarizer`:
CLAUDE_BIN="${CLAUDE_BIN:-/opt/homebrew/bin/claude}"
CLAUDE_MODEL="${CLAUDE_MODEL:-claude-haiku-4-5-20251001}"

set -eu
MODEL="$CLAUDE_MODEL"
case "${MUNSHI_SUMMARIZER_PHASE:-complete}" in
  chunk) MODEL="${MUNSHI_CHUNK_MODEL:-$CLAUDE_MODEL}" ;;
  reduce) MODEL="${MUNSHI_REDUCE_MODEL:-$CLAUDE_MODEL}" ;;
esac
"$CLAUDE_BIN" -p --model "$MODEL" \
  --append-system-prompt "Respond with exactly one JSON object matching required_schema." \
  | python3 -c '
import json, re, sys
text = sys.stdin.read().strip()
text = re.sub(r"^```(?:json)?\s*|\s*```$", "", text)
summary = json.loads(text)
for key, value in summary.items():
    if isinstance(value, list) and not value:
        summary[key] = ["none"]
sys.stdout.write(json.dumps(summary))
'
