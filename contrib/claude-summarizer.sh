#!/bin/sh
# Munshi summarizer wrapper around Claude Code (`claude -p`).
#
# Bare `claude -p` is not usable as a Munshi summarizer directly: it tends to wrap its JSON
# answer in a Markdown code fence, and for trivial sessions it returns empty arrays for list
# fields, both of which fail Munshi's StructuredSummary validation (docs/summarizers.md).
# This wrapper strips any fence and backfills empty lists with ["none"].
#
# It also isolates Claude Code's own session recording, for the same reason
# contrib/copilot-summarizer.sh isolates COPILOT_HOME (issue #37): every `claude -p` run is a
# Claude Code session, and a session recorded inside the registered Claude home is discovered by
# Munshi's hooks and recovery sweep as new archive-worthy work — the summary request is a user
# message and the summary is an assistant reply — so archiving N sessions creates N more. Three
# independent measures, in increasing order of what they assume:
#
#   1. `--no-session-persistence` — no transcript is written at all, so there is nothing for any
#      sweep to discover and no isolated home to grow without bound.
#   2. `--setting-sources ''` — no user/project/local settings are loaded, so Munshi's own Stop
#      and SessionEnd hooks (written into the Claude home's settings.json by `munshi register`)
#      never fire for a summarizer run.
#   3. CLAUDE_CONFIG_DIR — relocates the whole Claude home, so anything Claude Code does write
#      lands outside the registered home. Verified against claude 2.1.220: a custom
#      CLAUDE_CONFIG_DIR moves `projects/` (the transcripts), `sessions/`, `settings.json`, and
#      `.claude.json` into it.
#
# Measures 1 and 2 are unconditional and are what actually close the loop; 3 is conditional,
# because of an auth constraint worth stating plainly. With CLAUDE_CONFIG_DIR set, Claude Code
# reads its credential from `$CLAUDE_CONFIG_DIR/.credentials.json` and does NOT fall back to the
# macOS Keychain — verified on 2.1.220, where an isolated home seeded with a copy of the real
# `.claude.json` still reports "Not logged in" even though the login Keychain holds a valid
# `Claude Code-credentials` item. On a Keychain-backed macOS install there is therefore no
# on-disk credential to symlink, and relocating the home unconditionally would break the
# summarizer rather than isolate it. So this wrapper seeds the isolated home with symlinks to
# whatever the real home does keep on disk — `.claude.json` for account and config, plus
# `.credentials.json` where the install stores it there (Linux, or a home already logged in under
# its own config dir) — and relocates only once the isolated home can actually authenticate.
# Deliberately NOT seeded: `settings.json`, which is exactly where Munshi's hooks live. That
# mirrors the copilot wrapper's deliberately absent hooks directory.
#
# To get full home isolation on a Keychain-backed macOS install, authenticate the isolated home
# once — `CLAUDE_CONFIG_DIR="$HOME/.claude-summarizer" claude /login` — or export
# CLAUDE_CODE_OAUTH_TOKEN (see `claude setup-token`) or ANTHROPIC_API_KEY. The wrapper picks the
# isolated home up by itself from then on.
#
# Assumes a `claude` supporting `--no-session-persistence` and `--setting-sources` (both present
# in 2.1.220). An older binary fails loudly on the unknown flag rather than silently recording
# sessions into the registered home, which is the safer of the two failures.
#
# Phase-aware model selection (summarizer contract v2, issue #48): Munshi exports
# MUNSHI_SUMMARIZER_PHASE=complete|chunk|reduce on every invocation. The optional
# MUNSHI_CHUNK_MODEL and MUNSHI_REDUCE_MODEL environment variables select a different Claude
# model for the chunk / reduce invocations of a chunked marathon session; when they are unset
# (or the phase variable is absent, under a pre-v2 Munshi) every invocation uses CLAUDE_MODEL
# exactly as before.
#
# Mid-run claim-ticket redemption (issue #25): opt-in via MUNSHI_TICKET_REDEMPTION=1 (set it
# through `munshi register --summarizer-env`). When enabled, the Claude agent is allowed to run
# exactly one command shape — `munshi retrieve … --local` — to read elided oversized events
# (`[munshi claim-ticket sha256:… bytes:… label:…]` markers) from the session's own on-disk
# transcript while it summarizes. Redemption is local-only by design: at summarize time the
# snapshot has not been uploaded yet, so the transcript is the only place the bytes exist; a
# local-only tool surface also keeps the summarizer off the network for content. The permission
# stays narrowly scoped (`--allowedTools "Bash(<munshi> retrieve:*)"` with settings still not
# loaded), so Munshi's hooks remain disabled and no other tool is available. Redemption spends
# summarizer wall-clock: raise `--timeout-ms` when enabling this on sessions with many tickets.
# MUNSHI_BIN overrides the munshi binary (defaults to `munshi` on PATH); MUNSHI_STATE_DIR is
# passed through as `--state-dir` for a non-default state directory.
#
# Adjust these two defaults for your machine (or override them via environment), then pass
# this script's absolute path to `munshi register --summarizer`:
CLAUDE_BIN="${CLAUDE_BIN:-/opt/homebrew/bin/claude}"
CLAUDE_MODEL="${CLAUDE_MODEL:-claude-haiku-4-5-20251001}"
REAL_HOME="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
ISOLATED_HOME="${MUNSHI_CLAUDE_SUMMARIZER_HOME:-$HOME/.claude-summarizer}"

set -eu
# `.claude.json` carries the account and per-project config. It sits beside the home by default
# ($HOME/.claude.json) but moves inside a home that CLAUDE_CONFIG_DIR has already relocated.
if [ -n "${CLAUDE_CONFIG_DIR:-}" ]; then
    REAL_CONFIG_JSON="$CLAUDE_CONFIG_DIR/.claude.json"
else
    REAL_CONFIG_JSON="$HOME/.claude.json"
fi
if [ ! -d "$ISOLATED_HOME" ]; then
    mkdir -p "$ISOLATED_HOME"
    [ -e "$REAL_CONFIG_JSON" ] && ln -s "$REAL_CONFIG_JSON" "$ISOLATED_HOME/.claude.json"
    [ -e "$REAL_HOME/.credentials.json" ] &&
        ln -s "$REAL_HOME/.credentials.json" "$ISOLATED_HOME/.credentials.json"
fi
if [ -e "$ISOLATED_HOME/.credentials.json" ] ||
    [ -n "${CLAUDE_CODE_OAUTH_TOKEN:-}" ] ||
    [ -n "${ANTHROPIC_API_KEY:-}" ]; then
    CLAUDE_CONFIG_DIR="$ISOLATED_HOME"
    export CLAUDE_CONFIG_DIR
fi
MODEL="$CLAUDE_MODEL"
case "${MUNSHI_SUMMARIZER_PHASE:-complete}" in
  chunk) MODEL="${MUNSHI_CHUNK_MODEL:-$CLAUDE_MODEL}" ;;
  reduce) MODEL="${MUNSHI_REDUCE_MODEL:-$CLAUDE_MODEL}" ;;
esac
SYSTEM_PROMPT="Respond with exactly one JSON object matching required_schema."
REDEEM_TOOLS=""
if [ -n "${MUNSHI_TICKET_REDEMPTION:-}" ]; then
    MUNSHI_BIN="${MUNSHI_BIN:-$(command -v munshi || true)}"
    if [ -n "$MUNSHI_BIN" ]; then
        STATE_ARG=""
        [ -n "${MUNSHI_STATE_DIR:-}" ] && STATE_ARG=" --state-dir $MUNSHI_STATE_DIR"
        REDEEM_TOOLS="Bash($MUNSHI_BIN retrieve:*)"
        SYSTEM_PROMPT="$SYSTEM_PROMPT Oversized elided events appear as markers: [munshi claim-ticket sha256:<hex> bytes:<n> label:<kind>]. To read one that matters to the summary, run: $MUNSHI_BIN retrieve <hex> --local --session <session.id from the request JSON>$STATE_ARG. Mind bytes:<n>: for large content add --query <term> to search inside it instead of printing it all. Redeem only tickets whose content you actually need; never invent elided content."
    fi
fi
set -- -p --model "$MODEL" \
  --no-session-persistence \
  --setting-sources '' \
  --append-system-prompt "$SYSTEM_PROMPT"
[ -n "$REDEEM_TOOLS" ] && set -- "$@" --allowedTools "$REDEEM_TOOLS"
"$CLAUDE_BIN" "$@" \
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
