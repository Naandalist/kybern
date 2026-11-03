#!/usr/bin/env bash
# Check that kybern-keychain is signed by the release certificate: its leaf certificate's SHA-1 must be the one in the
# repository variable MACOS_KEYCHAIN_HELPER_CERT_SHA1, which a pull request cannot change (README.md, "Checks").
#
#   verify-signer.sh <expected SHA-1>
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
expected="$(printf '%s' "${1:-}" | tr -d ' :' | tr '[:lower:]' '[:upper:]')"
if [[ -z "$expected" ]]; then
  echo "::error::MACOS_KEYCHAIN_HELPER_CERT_SHA1 is not set; set the repository variable before packaging" >&2
  exit 1
fi
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
codesign --verify --strict kybern-keychain
codesign -d --extract-certificates="$WORK/cert" kybern-keychain 2>/dev/null
[[ -f "$WORK/cert0" ]] || { echo "::error::kybern-keychain carries no certificate" >&2; exit 1; }
actual="$(shasum -a 1 "$WORK/cert0" | cut -d' ' -f1 | tr '[:lower:]' '[:upper:]')"
identifier="$(codesign -d --verbose=2 kybern-keychain 2>&1 | sed -n 's/^Identifier=//p')"
if [[ "$actual" != "$expected" || "$identifier" != "dev.kybern.keychain" ]]; then
  echo "::error::kybern-keychain is signed by certificate $actual as $identifier, not the release certificate $expected as dev.kybern.keychain" >&2
  exit 1
fi
echo "kybern-keychain is signed by the release certificate ($actual)"
