#!/bin/sh
# Munshi summarizer wrapper around Copilot CLI (`copilot -s`).
#
# Bare `copilot -s --no-ask-user` is not usable as a Munshi summarizer for every session: for
# trivial sessions it returns empty arrays for list fields (most often `work_completed`), and it
# may wrap the JSON answer in a Markdown code fence — both fail Munshi's StructuredSummary
# validation (docs/summarizers.md). This wrapper strips any fence and backfills empty lists with
# ["none"], mirroring contrib/claude-summarizer.sh.
#
# Adjust this line for your machine (must be the resolved binary, not a symlink), then pass this
# script's absolute path to `munshi register --summarizer`:
COPILOT_BIN="/opt/homebrew/Caskroom/copilot-cli/1.0.59/copilot"

set -eu
"$COPILOT_BIN" -s --no-ask-user \
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
