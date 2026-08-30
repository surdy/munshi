#!/usr/bin/env bash
# build-gui.sh — package the Munshi desktop addon (docs/gui.md, ADR 0014).
#
# The app ships the CLI inside its own bundle, so this builds `munshi` first, stages it as a
# Tauri resource, and only then builds the app. The staged copy is what the app's "Install the
# munshi command" button writes to ~/.local/bin.
#
# On macOS the staged CLI is signed with the same stable identity contrib/dev-deploy.sh uses,
# before the app is bundled around it. That matters beyond the usual signing reasons: the launchd
# tick runs munshi in the background, where TCC attributes folder access to the binary itself and
# keys the grant to its designated requirement. An ad-hoc binary has none, so TCC falls back to a
# cdhash that changes on every rebuild and re-prompts. Signing the copy that gets installed keeps
# those grants across upgrades of this app.
#
# Usage:  ./contrib/build-gui.sh [--debug]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GUI_DIR="$REPO_ROOT/gui"
STAGED="$GUI_DIR/src-tauri/resources/bin/munshi"
SIGN_ID="${MUNSHI_SIGN_IDENTITY:-munshi-dev}"

MODE="release"
TAURI_ARGS=()
if [[ "${1:-}" == "--debug" ]]; then
  MODE="debug"
  TAURI_ARGS+=(--debug)
fi

echo "==> Building the munshi CLI ($MODE)"
if [[ "$MODE" == "release" ]]; then
  cargo build --release -p munshi --manifest-path "$REPO_ROOT/Cargo.toml"
  CLI_BUILT="$REPO_ROOT/target/release/munshi"
else
  cargo build -p munshi --manifest-path "$REPO_ROOT/Cargo.toml"
  CLI_BUILT="$REPO_ROOT/target/debug/munshi"
fi

echo "==> Staging the CLI at resources/bin/munshi"
mkdir -p "$(dirname "$STAGED")"
install -m 755 "$CLI_BUILT" "$STAGED"

if [[ "$(uname)" == "Darwin" ]]; then
  # Fall back to ad-hoc if the stable identity is missing, with the same loud warning
  # contrib/dev-deploy.sh gives — a working build with orphaned TCC grants beats no build.
  if ! security find-identity -v -p codesigning | grep -q "\"$SIGN_ID\""; then
    echo "WARNING: signing identity '$SIGN_ID' not found; falling back to ad-hoc (-)." >&2
    echo "         TCC grants will NOT persist across rebuilds. See contrib/dev-deploy.sh." >&2
    SIGN_ID="-"
  fi
  echo "==> Signing the staged CLI (identity: $SIGN_ID)"
  codesign --force --sign "$SIGN_ID" --identifier com.munshi --timestamp=none "$STAGED"
  codesign --verify --strict "$STAGED"
fi

if [[ ! -d "$GUI_DIR/node_modules" ]]; then
  echo "==> Installing frontend dependencies"
  (cd "$GUI_DIR" && npm install --no-audit --no-fund)
fi

echo "==> Building the app bundle"
(cd "$GUI_DIR" && npx tauri build "${TAURI_ARGS[@]}")

echo
echo "Done. Bundles are under gui/src-tauri/target/$MODE/bundle/."
