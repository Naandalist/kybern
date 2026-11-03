# kybern-keychain

The only program that reads or writes Kybern GPUIX's Keychain items on macOS. The app runs it
(`src/environments/credentials.ts`) with the `kybern credentials` contract:
`kybern-keychain credentials get|set|delete --service <s> --account <a>`, the secret on stdin and
stdout, exit 3 for no item and 4 for locked or declined. All environment tokens live in one item,
account `all` (a JSON map of id → token, `src/environments/combinedKeychain.ts`).

## Why a separate, committed binary

The login keychain gives every item two gates:

- the access list, which names the creating program by its designated requirement (DR), and
- a partition list, which for any program without an Apple-issued Team ID is `cdhash:<hash>`
  of that exact build.

A reader that fails either gate gets "… wants to use your confidential information stored in …"
and has to type the keychain password. Up to 0.21.3 the app read its items in process and was
signed ad hoc, so both gates changed with every release: each update asked again, once per
environment item, even after "Always Allow".

A stable self-signed certificate fixes the first gate only. Measured on macOS 27 with the scratch
service: two builds signed with "Kybern Code Signing" have the same DR, but the second is still
refused (`-25293`) because its cdhash differs from the partition the first one recorded. A Team ID
cannot be faked either: `codesign --team-identifier` with a self-signed certificate produces a
binary the kernel kills on launch.

So the Keychain is reached through a program whose bytes never change. Its cdhash, and therefore the
partition of every item it creates, stays the same from release to release, and the items trust it
without asking. The app's own executable can change freely.

## Rules

- Do not rebuild, strip or re-sign `kybern-keychain` without a reason. A new binary has a new
  cdhash: every user is asked once more (one prompt for the combined item), then never again.
  `package-app.ts` copies it as is and fails if its cdhash is not the one in `pin.json`;
  `credentials.test.ts` checks its sha256.
- Keep the source minimal for the same reason. Logic belongs in the app (`combinedKeychain.ts`).
- To rebuild on purpose: set up the signing identity (`eval "$(scripts/macos-signing.sh setup)"`
  with the .p12 secrets), run `./build.sh`, commit the binary and the new `pin.json`, and say in the
  release notes that macOS asks once for the keychain password. The checks below must still pass.

## Checks

A change cannot swap the binary on its own:

- `reproduce.sh` rebuilds the helper from `kybern-keychain.c` (`build.sh --unsigned`) and compares it with the
  committed binary, both with their code signatures removed: every byte of every architecture except `LC_UUID`,
  which the linker derives from temporary object file paths, and the `__LINKEDIT` vmsize, which still counts the
  removed signature (`macho_normalize.py`). It needs the Xcode the helper was linked with (27.0; it checks the SDK
  and linker version first). CI runs it on the `xcode-27` runner for every change under `apps/gpuix`, and the
  Desktop workflow before it packages macOS.
- `verify-signer.sh` checks that the helper is signed as `dev.kybern.keychain` by the release certificate: the
  SHA-1 of its leaf certificate must equal the repository variable `MACOS_KEYCHAIN_HELPER_CERT_SHA1`, which a
  pull request cannot change. The Desktop workflow runs it before packaging; packaging fails on a mismatch.
- `.github/CODEOWNERS` asks for the owner's review of anything in this folder.

After rebuilding with Xcode other than 27.0, update the Xcode in `reproduce.sh`'s callers (`ci.yml`, `desktop.yml`).
After rotating the certificate, set the variable to the new certificate's SHA-1
(`gh variable set MACOS_KEYCHAIN_HELPER_CERT_SHA1 --body <sha1>`).

## First update to a release with the helper

The items older releases made (one per environment, `machine:<id>` and profile ids) trust only the
old app build, so the helper is asked about each one the first time it is read: one prompt per old
item, one time. After the token is copied into the combined item, the old item is deleted. If a
prompt is declined, that item stays and is asked about on the next launch; a token is never dropped.

## Rotating the certificate

The certificate is valid until 2046. To replace it, make a new one with the same properties (CN
"Kybern Code Signing", `basicConstraints CA:FALSE`, `keyUsage digitalSignature`,
`extendedKeyUsage codeSigning`), store it as `MACOS_SIGNING_P12_BASE64` /
`MACOS_SIGNING_P12_PASSWORD`, and rebuild the helper with it (above). The app's DR changes, and so
does the helper's cdhash: users see one more round of prompts for the combined item, and macOS asks
again for privacy permissions granted to the app.

## Known limits

- The helper does not check who runs it. Any program running as the user can ask it for the tokens
  without a prompt, as it can already run the app's own executable with `BUN_BE_BUN=1` and the
  app's Keychain access. The Keychain protects these tokens from other users and from a locked
  session, not from code the user runs.
- `kybern credentials` (the CLI) also reaches the Keychain on macOS; it is signed with the same
  certificate but changes with every release, so items it creates by hand still prompt after an
  update. The app no longer uses it on macOS.
