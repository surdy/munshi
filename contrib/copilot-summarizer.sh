#!/bin/sh
# Munshi summarizer wrapper around Copilot CLI (`copilot -s`).
#
# Bare `copilot -s --no-ask-user` is not usable as a Munshi summarizer for every session: for
# trivial sessions it returns empty arrays for list fields (most often `work_completed`), and it
# may wrap the JSON answer in a Markdown code fence — both fail Munshi's StructuredSummary
# validation (docs/summarizers.md). This wrapper strips any fence and backfills empty lists with
# ["none"], mirroring contrib/claude-summarizer.sh.
#
# It also isolates COPILOT_HOME: every `copilot -s` run creates a fresh Copilot session, and if
# those land in the registered home, Munshi's hooks and recovery sweep discover the summarizer's
# own sessions as new archive-worthy work — a self-feeding loop that summarizes its own exhaust.
# The isolated home is seeded once with symlinks to the real home's config/auth, but gets its own
# session-state and, deliberately, NO hooks directory.
#
# Adjust this line for your machine (must be the resolved binary, not a symlink), then pass this
# script's absolute path to `munshi register --summarizer`:
COPILOT_BIN="/opt/homebrew/Caskroom/copilot-cli/1.0.59/copilot"
REAL_HOME="${COPILOT_HOME:-$HOME/.copilot}"
ISOLATED_HOME="$HOME/.copilot-summarizer"

set -eu
if [ ! -d "$ISOLATED_HOME" ]; then
    mkdir -p "$ISOLATED_HOME/session-state"
    for entry in config config.json mcp-config.json permissions-config.json servers; do
        [ -e "$REAL_HOME/$entry" ] && ln -s "$REAL_HOME/$entry" "$ISOLATED_HOME/$entry"
    done
fi
COPILOT_HOME="$ISOLATED_HOME" XDG_CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}" \
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
