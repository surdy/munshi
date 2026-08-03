#!/usr/bin/env bash
# dev-deploy.sh — build munshi, sign it with a STABLE local identity (macOS),
# and install it to ~/.local/bin.
#
# Why a stable identity?
#   The launchd tick (contrib/launchd/com.munshi.tick.plist) runs munshi in the
#   background, where macOS TCC attributes folder access (Documents, Desktop, …)
#   to the munshi binary itself rather than the user's terminal. TCC keys the
#   grant to the binary's designated requirement; an ad-hoc (or linker-signed)
#   binary has none, so TCC falls back to the raw cdhash — which changes on
#   every rebuild, orphaning the grant and re-prompting. Signing with a
#   self-signed cert whose subject never changes keeps grants across rebuilds.
#   (Proper fix — tick not touching session cwds at all — is issue #61.)
#
# One-time cert setup (already done if "munshi-dev" appears in
# `security find-identity -v -p codesigning`). To (re)create it:
#   openssl req -x509 -newkey rsa:2048 -keyout munshi-dev.key -out munshi-dev.crt \
#     -days 3650 -nodes -subj /CN=munshi-dev \
#     -addext keyUsage=critical,digitalSignature \
#     -addext extendedKeyUsage=critical,codeSigning \
#     -addext basicConstraints=critical,CA:false
#   openssl pkcs12 -export -legacy -out munshi-dev.p12 -inkey munshi-dev.key \
#     -in munshi-dev.crt -password pass:tmp
#   security import munshi-dev.p12 -k ~/Library/Keychains/login.keychain-db \
#     -P tmp -T /usr/bin/codesign
#   security add-trusted-cert -p codeSign \
#     -k ~/Library/Keychains/login.keychain-db munshi-dev.crt
#   rm munshi-dev.key munshi-dev.p12 munshi-dev.crt
#
# Usage:  ./contrib/dev-deploy.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SIGN_ID="${MUNSHI_SIGN_IDENTITY:-munshi-dev}"
DST="$HOME/.local/bin/munshi"

echo "==> Building munshi (release)"
cargo build --release -p munshi --manifest-path "$REPO_ROOT/Cargo.toml"

echo "==> Installing to $DST"
install -m 755 "$REPO_ROOT/target/release/munshi" "$DST"

if [[ "$(uname)" == "Darwin" ]]; then
  # Fall back to ad-hoc if the stable identity is missing (with a loud warning).
  if ! security find-identity -v -p codesigning | grep -q "\"$SIGN_ID\""; then
    echo "WARNING: signing identity '$SIGN_ID' not found; falling back to ad-hoc (-)." >&2
    echo "         TCC grants will NOT persist across rebuilds. Create the cert to fix." >&2
    SIGN_ID="-"
  fi
  echo "==> Signing (identity: $SIGN_ID)"
  codesign --force --sign "$SIGN_ID" --identifier com.munshi --timestamp=none "$DST"
  codesign --verify --strict "$DST"
fi

"$DST" --version 2>/dev/null || true
echo "Done."
