#!/usr/bin/env bash
# Rebuild the committed kybern-keychain helper (read README.md first: a new binary means one more round of Keychain
# prompts for every user). Signs with KYBERN_CODESIGN_IDENTITY from KYBERN_CODESIGN_KEYCHAIN (scripts/macos-signing.sh
# sets both up from the code-signing .p12), then rewrites pin.json.
#
#   ./build.sh                    build, sign and pin the committed binary
#   ./build.sh --unsigned <file>  only compile and strip into <file> (reproduce.sh compares it with the committed one)
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
compile() {
  clang -arch arm64 -arch x86_64 -mmacosx-version-min=13.0 -Os -Wall -Werror -Wno-deprecated-declarations \
    -framework Security -framework CoreFoundation kybern-keychain.c -o "$1"
  strip -x "$1"
}
if [[ "${1:-}" == "--unsigned" ]]; then
  compile "${2:?usage: build.sh --unsigned <file>}"
  exit 0
fi
: "${KYBERN_CODESIGN_IDENTITY:?set KYBERN_CODESIGN_IDENTITY (scripts/macos-signing.sh setup)}"
compile kybern-keychain
codesign --force --timestamp=none --identifier dev.kybern.keychain --sign "$KYBERN_CODESIGN_IDENTITY" \
  ${KYBERN_CODESIGN_KEYCHAIN:+--keychain "$KYBERN_CODESIGN_KEYCHAIN"} kybern-keychain
cdhash() { codesign -d --verbose=3 --arch "$1" kybern-keychain 2>&1 | sed -n 's/^CDHash=//p'; }
cat > pin.json <<JSON
{
  "sha256": "$(shasum -a 256 kybern-keychain | cut -d' ' -f1)",
  "cdhash": { "arm64": "$(cdhash arm64)", "x86_64": "$(cdhash x86_64)" }
}
JSON
codesign -d -r- kybern-keychain
cat pin.json
