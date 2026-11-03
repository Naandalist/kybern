#!/usr/bin/env bash
# Rebuild kybern-keychain from kybern-keychain.c and check that the committed binary is that build, so a change can
# only swap the helper together with its source (CI and the Desktop workflow run this; README.md, "Checks").
#
# Compared: both binaries with their code signatures removed, every byte of every architecture slice except LC_UUID
# (the linker derives it from temporary object file paths) and the __LINKEDIT vmsize (it still counts the removed
# signature): macho_normalize.py. The signature itself is checked by verify-signer.sh.
#
# Needs the Xcode the committed binary was linked with: its SDK and linker version (LC_BUILD_VERSION) must match.
# Set DEVELOPER_DIR to choose one (CI uses /Applications/Xcode_27.0.app on the xcode-27 runner).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

build_version() { otool -arch arm64 -l "$1" | awk '/cmd LC_BUILD_VERSION/ { found = 1 } found && /sdk|version/ { printf "%s %s ", $1, $2 } found && /version/ { exit }'; }
cp kybern-keychain "$WORK/committed"
codesign --remove-signature "$WORK/committed"
./build.sh --unsigned "$WORK/rebuilt"
# The linker signs an arm64 slice ad hoc by itself.
codesign --remove-signature "$WORK/rebuilt"
want="$(build_version "$WORK/committed")"
have="$(build_version "$WORK/rebuilt")"
if [[ "$want" != "$have" ]]; then
  echo "kybern-keychain was linked with ($want), this Xcode links with ($have). Set DEVELOPER_DIR to the matching Xcode." >&2
  exit 1
fi
read -r committed _ < <(python3 -I macho_normalize.py "$WORK/committed")
read -r rebuilt _ < <(python3 -I macho_normalize.py "$WORK/rebuilt")
if [[ "$committed" != "$rebuilt" ]]; then
  echo "::error::The committed kybern-keychain is not the build of kybern-keychain.c ($committed, rebuilt $rebuilt)." >&2
  exit 1
fi
echo "kybern-keychain matches its source ($want; normalized sha256 $committed)"
