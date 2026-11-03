#!/usr/bin/env bash
# The stable macOS code-signing identity ("Kybern Code Signing", self-signed, no Apple Developer ID) in a throwaway
# keychain, so release builds keep one designated requirement from version to version (apps/gpuix/packaging/keychain/
# README.md explains why that matters).
#
#   eval "$(scripts/macos-signing.sh setup)"   # prints the KYBERN_CODESIGN_* exports
#   scripts/macos-signing.sh cleanup           # deletes the keychain (and restores the search list)
#
# setup reads MACOS_SIGNING_P12_BASE64 (the .p12, base64) and MACOS_SIGNING_P12_PASSWORD. Without them it prints
# nothing and exits 0: packaging then signs ad hoc and says so. Neither the key nor a password is ever printed.
set -euo pipefail

KEYCHAIN="${KYBERN_SIGNING_KEYCHAIN:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}/kybern-signing.keychain-db}"
SEARCH_LIST="$KEYCHAIN.search-list"

case "${1:-}" in
setup)
  if [[ -z "${MACOS_SIGNING_P12_BASE64:-}" || -z "${MACOS_SIGNING_P12_PASSWORD:-}" ]]; then
    echo "macos-signing: MACOS_SIGNING_P12_BASE64 is not set; packaging signs ad hoc" >&2
    exit 0
  fi
  WORK="$(mktemp -d)"
  trap 'rm -rf "$WORK"' EXIT
  umask 077
  printf '%s' "$MACOS_SIGNING_P12_BASE64" | base64 --decode > "$WORK/signing.p12"
  PASS="$(openssl rand -hex 24)"
  security delete-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
  security create-keychain -p "$PASS" "$KEYCHAIN"
  security set-keychain-settings -lut 21600 "$KEYCHAIN"
  security unlock-keychain -p "$PASS" "$KEYCHAIN"
  security import "$WORK/signing.p12" -k "$KEYCHAIN" -f pkcs12 -P "$MACOS_SIGNING_P12_PASSWORD" -T /usr/bin/codesign >/dev/null
  # codesign may use the key without a prompt.
  security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$PASS" "$KEYCHAIN" >/dev/null
  # codesign is pointed at this keychain with --keychain. macOS 15 (the runners) still finds the identity only on the
  # search list, so CI puts it first there; cleanup restores the old list. Elsewhere set KYBERN_SIGNING_SEARCH_LIST=1.
  if [[ -n "${CI:-}" || "${KYBERN_SIGNING_SEARCH_LIST:-0}" == "1" ]]; then
    security list-keychains -d user | sed -e 's/^ *"//' -e 's/" *$//' > "$SEARCH_LIST"
    OLD=()
    while IFS= read -r item; do [[ -n "$item" ]] && OLD+=("$item"); done < "$SEARCH_LIST"
    security list-keychains -d user -s "$KEYCHAIN" ${OLD[@]+"${OLD[@]}"}
  fi
  IDENTITY="$(security find-identity -p codesigning "$KEYCHAIN" | awk '/"Kybern Code Signing"/ { print $2; exit }')"
  if [[ -z "$IDENTITY" ]]; then
    echo "macos-signing: the .p12 holds no \"Kybern Code Signing\" identity" >&2
    exit 1
  fi
  echo "macos-signing: signing with \"Kybern Code Signing\" ($IDENTITY)" >&2
  echo "export KYBERN_CODESIGN_IDENTITY=$IDENTITY"
  printf 'export KYBERN_CODESIGN_KEYCHAIN=%q\n' "$KEYCHAIN"
  ;;
cleanup)
  if [[ -f "$SEARCH_LIST" ]]; then
    OLD=()
    while IFS= read -r item; do [[ -n "$item" ]] && OLD+=("$item"); done < "$SEARCH_LIST"
    security list-keychains -d user -s ${OLD[@]+"${OLD[@]}"}
    rm -f "$SEARCH_LIST"
  fi
  security delete-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
  ;;
*)
  echo "usage: scripts/macos-signing.sh setup|cleanup" >&2
  exit 2
  ;;
esac
