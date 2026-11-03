#!/usr/bin/env bash
# Package Kybern GPUIX (apps/gpuix) as a macOS app and write the updater
# tarball that latest.json points at: dist/kybern-gpuix_<version>_<arch>.app.tar.gz
# (arch aarch64 or x86_64) plus its minisign .sig, signed with the updater key
# (the one the retired Tauri app used).
#
# Usage: scripts/bundle-gpuix.sh
#   KYBERN_GPUIX_TARGET         aarch64-apple-darwin (default on Apple Silicon) or
#                               x86_64-apple-darwin (Intel; cross-built from arm64)
#   KYBERN_BIN_DIR              the folder holding that target's kybernd and kybern
#                               (releases pass dist's binaries); defaults to
#                               $CARGO_TARGET_DIR/release, or $CARGO_TARGET_DIR/<target>/release
#                               when cross-building
#   CARGO_TARGET_DIR            honoured if set
#   TAURI_SIGNING_PRIVATE_KEY   required unless SKIP_SIGN=1 (the updater key)
#   MACOS_SIGNING_P12_BASE64,   the "Kybern Code Signing" identity (scripts/macos-signing.sh), which keeps the app's
#   MACOS_SIGNING_P12_PASSWORD  designated requirement the same across versions; without them the app is signed ad hoc
#                               and every update looks like a new app to the Keychain
#
# Requires: the patched native addon for that target (apps/gpuix/native/update.sh
# with the same KYBERN_GPUIX_TARGET), release builds of kybern and kybernd for
# it, bun, and `pnpm install` in apps/gpuix (mermaid and the Tauri CLI's
# updater signer).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "bundle-gpuix.sh must run on macOS" >&2
  exit 1
fi

HOST="$(rustc -vV | sed -n 's/^host: //p')"
TARGET="${KYBERN_GPUIX_TARGET:-$HOST}"
case "$TARGET" in
  aarch64-apple-darwin) ARCH=aarch64 BUN_ARCH=arm64 ;;
  x86_64-apple-darwin) ARCH=x86_64 BUN_ARCH=x64 ;;
  *) echo "unsupported GPUIX target: $TARGET" >&2; exit 1 ;;
esac
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
DEFAULT_BIN_DIR="$TARGET_DIR/release"
[[ "$TARGET" == "$HOST" ]] || DEFAULT_BIN_DIR="$TARGET_DIR/$TARGET/release"
BIN_DIR="$(cd "${KYBERN_BIN_DIR:-$DEFAULT_BIN_DIR}" && pwd)"
DIST="$ROOT/dist"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
TARBALL="$DIST/kybern-gpuix_${VERSION}_${ARCH}.app.tar.gz"

if [[ -z "${KYBERN_CODESIGN_IDENTITY:-}" && -n "${MACOS_SIGNING_P12_BASE64:-}" ]]; then
  trap '"$ROOT/scripts/macos-signing.sh" cleanup' EXIT
  SIGNING="$("$ROOT/scripts/macos-signing.sh" setup)"
  eval "$SIGNING"
  [[ -n "${KYBERN_CODESIGN_IDENTITY:-}" ]] || { echo "the code-signing identity could not be set up" >&2; exit 1; }
fi

echo "==> packaging Kybern GPUIX $VERSION for $TARGET"
rm -rf apps/gpuix/dist
(cd apps/gpuix && KYBERN_BIN_DIR="$BIN_DIR" bun scripts/package-app.ts --arch "$BUN_ARCH")
APP="$ROOT/apps/gpuix/dist/Kybern GPUIX.app"

echo "==> $TARBALL"
mkdir -p "$DIST"
rm -f "$TARBALL" "$TARBALL.sig"
COPYFILE_DISABLE=1 tar -czf "$TARBALL" -C "$(dirname "$APP")" "Kybern GPUIX.app"

if [[ "${SKIP_SIGN:-0}" == "1" ]]; then
  echo "==> SKIP_SIGN=1; leaving the tarball unsigned"
  exit 0
fi
if [[ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" ]]; then
  echo "TAURI_SIGNING_PRIVATE_KEY is not set" >&2
  exit 1
fi
(cd apps/gpuix && node node_modules/@tauri-apps/cli/tauri.js signer sign "$TARBALL")
(cd apps/gpuix && bun scripts/verify-archive.ts "$TARBALL")
echo "done: $TARBALL"
