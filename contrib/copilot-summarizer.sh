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
# Phase-aware model selection (summarizer contract v2, issue #48): Munshi exports
# MUNSHI_SUMMARIZER_PHASE=complete|chunk|reduce on every invocation. The optional
# MUNSHI_CHUNK_MODEL and MUNSHI_REDUCE_MODEL environment variables select a Copilot CLI model
# (`--model <model>`, supported by copilot 1.0.59 per its `--help`) for the chunk / reduce
# invocations of a chunked marathon session; when they are unset (or the phase variable is
# absent, under a pre-v2 Munshi) no `--model` flag is passed and Copilot's configured default
# model applies, exactly as before.
#
# Adjust this default for your machine (must be the resolved binary, not a symlink; overridable
# via environment), then pass this script's absolute path to `munshi register --summarizer`:
COPILOT_BIN="${COPILOT_BIN:-/opt/homebrew/Caskroom/copilot-cli/1.0.59/copilot}"
REAL_HOME="${COPILOT_HOME:-$HOME/.copilot}"
ISOLATED_HOME="$HOME/.copilot-summarizer"

set -eu
if [ ! -d "$ISOLATED_HOME" ]; then
    mkdir -p "$ISOLATED_HOME/session-state"
    for entry in config config.json mcp-config.json permissions-config.json servers; do
        [ -e "$REAL_HOME/$entry" ] && ln -s "$REAL_HOME/$entry" "$ISOLATED_HOME/$entry"
    done
fi
MODEL=""
case "${MUNSHI_SUMMARIZER_PHASE:-complete}" in
  chunk) MODEL="${MUNSHI_CHUNK_MODEL:-}" ;;
  reduce) MODEL="${MUNSHI_REDUCE_MODEL:-}" ;;
esac
if [ -n "$MODEL" ]; then
    set -- -s --no-ask-user --model "$MODEL"
else
    set -- -s --no-ask-user
fi
COPILOT_HOME="$ISOLATED_HOME" XDG_CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}" \
"$COPILOT_BIN" "$@" \
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
