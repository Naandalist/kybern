# Releasing

A push to `main` updates the source and runs CI; it does not publish a new
installed-app version. Relevant `main` changes also warm the build caches
that release builds restore. Publishing is a separate action.

| Release unit | Version and delivery |
| --- | --- |
| Daemon, CLI, and desktop | Share the Rust workspace version, which GPUIX's packaging reads. `scripts/release.sh X.Y.Z` updates it and pushes `vX.Y.Z`. The release workflow builds `kybernd` and `kybern` once per target; the desktop bundles, which include the daemon and CLI, reuse those binaries, and the whole GitHub Release is published at once. The `gpuix` job of `desktop.yml` builds Kybern GPUIX (`apps/gpuix`) for Apple Silicon and Intel Macs from the same dist binaries and attaches `kybern-gpuix_<version>_<aarch64|x86_64>.app.tar.gz` plus their signatures; `latest.json` carries them as `darwin-aarch64-gpuix` and `darwin-x86_64-gpuix`. The separate `gpuix-windows` job (beta) builds the Windows x64 installer `kybern-gpuix_<version>_x64-setup.exe` (`scripts/bundle-gpuix-windows.sh`) and its signature, published as `windows-x86_64-gpuix`; the manifest job runs whether that job succeeds or fails, so a Windows failure only leaves that entry out. The separate `gpuix-linux` job (beta) builds the Linux x86_64 packages on `ubuntu-22.04` from dist's static musl binaries: `kybern-gpuix_<version>_x86_64.AppImage` (signed; `latest.json` key `linux-x86_64-gpuix`), `.deb` and `.rpm`; like Windows, a Linux failure only leaves that entry out. |
| Android and iOS mobile app | Use `expo.version` in `apps/mobile/app.json`, independent of the desktop release number. EAS builds use the selected commit and profile. Android `versionCode` and iOS `buildNumber` identify individual binaries; EAS manages these remotely. Preview and production builds increment them automatically. |
| Wire protocol | `PROTOCOL_VERSION` is a compatibility contract, not an app release number. Bump it for breaking wire changes and update both clients and the daemon together. Additive features may still require a newer daemon even when the protocol number is unchanged. |

## Update tracks

Desktop installs and self-updating daemons follow one of two tracks, the
`update_track` setting (Settings → About → Update track; stored by the daemon,
shared across the machine network, Stable for new installs):

| Track | Gets | Feed |
| --- | --- | --- |
| Dev | every `vX.Y.Z-dev.N` release, and stable releases | `latest-dev.json` on the rolling `dev-feed` release, plus `latest.json`; the newer wins |
| Stable | `vX.Y.Z` releases, each promoted from a dev build | `latest.json` of the latest GitHub release |

Versions compare as semver: `0.21.0-dev.2` is newer than `0.20.x`, older than
`0.21.0`, and `dev.10` is newer than `dev.9`. Nothing ever moves to an older
version, so a machine switched from Dev to Stable keeps its dev build until a
stable release is newer (the store also refuses a data folder a newer version
upgraded).

**Cut a dev release** (as often as you like, from `main`):

```sh
# Optional: a `## 0.21.0-dev.1` entry at the top of CHANGELOG.md becomes the release notes.
scripts/release.sh 0.21.0-dev.1
```

The tag runs the same release workflow, builds and signing as a stable one.
dist publishes it as a GitHub prerelease (which `releases/latest` skips);
`scripts/updater-manifest.mjs` writes `latest-dev.json` instead of `latest.json`;
and dist's post-announce job `dev-feed.yml` copies that file to the `dev-feed`
prerelease (created on first use; never moved back to an older dev build). Dev
installs read
`https://github.com/Tresnanda/kybern/releases/download/dev-feed/latest-dev.json`.
The gate for a `-dev.N` tag only needs main's latest CI run for the tagged
commit to have succeeded; that run may be filtered, because a bump to a dev
version runs only the jobs its files affect (`scripts/ci-paths.mjs`). Number dev
builds of a version upwards (`-dev.1`, `-dev.2`, …) and move to the next
version's `-dev.1` after its stable release.

**Promote to stable** once a dev build has proven itself:

```sh
scripts/release.sh 0.21.0
```

`release.sh` refuses a stable version unless `main` matches the newest dev tag
apart from version lines and `CHANGELOG.md`: cut one more dev release first if
anything landed since. `--allow-unshipped` overrides that for an urgent fix. The
stable tag needs the full gate below.

## The release gate

The tag-triggered release workflow requires successful main CI for the exact
tagged commit, including all six jobs `scripts/check-release-ci.mjs` lists
(`requiredJobs`: rustfmt, clippy and test on Linux and macOS, Windows cargo check,
release checks, and gpuix); a dev tag (`vX.Y.Z-dev.N`) needs only that run's success.
A push that changes `[workspace.package].version` to a stable version runs the full suite
automatically. For a release candidate without a version change, run
`gh workflow run ci.yml --ref main` while `main` points at that candidate and
wait for it to pass before tagging. A manual run qualifies only for its exact
commit on `main`; a later commit's checks never qualify an older tag.
The gate waits for an in-progress run and blocks
publication on failures, skipped jobs, missing results, or timeout. A test that
failed once and passed on nextest's retry is quarantined (AGENTS.md, "Tests and
checks"): its job succeeds, so it does not block a release, and its `flaky`
issue tracks the fix. A test that fails both attempts still blocks. Publication
also requires both macOS GPUIX bundles to succeed (the Linux and Windows
bundles are betas whose failure only leaves their `latest.json` entry out). Only the publishing `host` job
waits for that gate; the daemon archives and desktop bundles build while CI
finishes. Renaming or adding a job in `ci.yml` means updating `requiredJobs`
and its test in the same change.

Pull requests and ordinary pushes to `main` run only the CI jobs their files
can affect (`scripts/ci-paths.mjs` lists the paths): a GPUIX-only change skips
the Rust jobs, and a docs-only change skips all test jobs. In a pull request, a
root `Cargo.toml` or `Cargo.lock` change that only moves the workspace version
counts as no change, so a version-bump PR runs only the jobs its other files
need, and the full suite runs once, on the bump's push to `main`. Main compares both
ends of the whole push, including deleted and renamed files. Version bumps on
main, manual runs, changes to CI selection/setup, and unavailable or incomplete
change lists run the full suite. A new push to a pull request cancels its running
checks; `main` runs are never cancelled. The `CI` job is red if any
job failed and green when the skipped ones were not needed, so it is the one
check a branch protection rule has to require.

Release builds restore caches that `main` saved and never save their own (a
cache saved under a tag cannot be restored by the next tag, and the repository's
cache storage is limited): `dist-cache.yml` builds the dist archives on a
`main` push that changes their dependencies, `gpuix-addon.yml` builds the
patched GPUIX native addon (both also run weekly, because GitHub deletes a
cache after seven days without use). `gpuix-wpe.yml` also preserves its
input-triggered and weekly warmer. Affected Rust and GPUIX checks on `main`
continue saving their Rust and pnpm caches; PR runs only restore them.

`latest.json` (and a dev release's `latest-dev.json`) lists only the `-gpuix` keys (`darwin-aarch64-gpuix`,
`darwin-x86_64-gpuix`, `windows-x86_64-gpuix`, `linux-x86_64-gpuix`). The plain
`darwin-aarch64`-style keys belonged to the retired Tauri app; releases after
0.18.0 no longer publish them, so Tauri installs stop updating at 0.18.0. The
updater signing secret keeps its `TAURI_SIGNING_PRIVATE_KEY` name, and the
`@tauri-apps/cli` dev dependency of `apps/gpuix` still signs the artifacts
(`signer sign`).

The macOS bundles are code-signed with the self-signed "Kybern Code Signing"
certificate from the `MACOS_SIGNING_P12_BASE64` and `MACOS_SIGNING_P12_PASSWORD`
secrets (`scripts/macos-signing.sh`), so every version keeps the designated
requirement `identifier "dev.kybern.gpuix" and certificate leaf = H"…"`. The
desktop workflow fails without them: an ad-hoc release would ask every user for
their keychain password again. It is not an Apple Developer ID; nothing is
notarized. Keychain access goes through the committed `kybern-keychain` helper
(`apps/gpuix/packaging/keychain/README.md`), which must not change between
releases.

Before running `scripts/release.sh X.Y.Z`, add a matching `## X.Y.Z` entry near
the top of `CHANGELOG.md` with concise, user-visible bullets. Cargo-dist copies
that entry into the GitHub Release, and the desktop updater uses its first bullet
as the announcement summary by default. Add these optional comments inside the
entry when the announcement needs a separate headline or shorter summary:

```md
<!-- kybern-release-title: Short feature headline -->
<!-- kybern-release-summary: One concrete sentence about the release. -->
```

A daemon-only fix is shipped in the next daemon/desktop release. A desktop-only
fix uses that same release unit because desktop bundles its daemon. A mobile-only
fix uses a compatible EAS Update or a mobile build, with no desktop version bump. A change spanning the
clients and daemon may require both release paths; publish the daemon support
before shipping a mobile feature that depends on it.

The desktop release script does not change the mobile app version or start EAS.
Mobile builds and updates are manual. Mobile 0.1.1 enables EAS Update on the
`preview` and `production` channels with fingerprint-based runtime compatibility.
Compatible JavaScript updates download at launch and apply on the next launch;
Settings → App updates also allows an explicit check and restart. Older APKs
need one new install to enable this. Native changes always require a new binary.
See the [mobile update commands](apps/mobile/README.md#eas). Do not introduce mobile
version tags without first narrowing the daemon workflow's broad tag trigger.

`release.yml` is generated by `dist generate` from `dist-workspace.toml`, with
one hand edit that `allow-dirty = ["ci"]` permits: `custom-desktop` does not
list `custom-release-ci` in `needs` (see the comment in the file). To refresh the
file, remove `allow-dirty`, run `dist generate`, restore the setting, and make
that edit again; `scripts/check-release-ci.test.mjs` fails if it is missing, and
`scripts/check-release-workflow.mjs` (run by CI) fails if the file differs from
`dist generate` anywhere else. It
runs `desktop.yml` as a dist custom job once every target's daemon and CLI
archive exists; that workflow bundles those binaries into the desktop
installers, writes `latest.json`, and hands everything to dist's host job,
which creates the release. Use `dist plan` to check the distribution config and
`dist generate` to refresh the generated workflow after changing it. Run
`desktop.yml` by hand (`workflow_dispatch`) for a dry run that builds the
archives and bundles without publishing; its `binaries_from` input reuses an
existing release's archives instead.
