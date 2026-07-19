#!/bin/sh
# Munshi summarizer wrapper around Claude Code (`claude -p`).
#
# Bare `claude -p` is not usable as a Munshi summarizer directly: it tends to wrap its JSON
# answer in a Markdown code fence, and for trivial sessions it returns empty arrays for list
# fields, both of which fail Munshi's StructuredSummary validation (docs/summarizers.md).
# This wrapper strips any fence and backfills empty lists with ["none"].
#
# Adjust these two lines for your machine, then pass this script's absolute path to
# `munshi register --summarizer`:
CLAUDE_BIN="/opt/homebrew/bin/claude"
CLAUDE_MODEL="claude-haiku-4-5-20251001"

set -eu
"$CLAUDE_BIN" -p --model "$CLAUDE_MODEL" \
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
