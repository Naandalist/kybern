# App packaging (macOS, Linux)

(Windows: see [Windows x64 (beta)](#windows-x64-beta) below.)

Build from `apps/gpuix` with `bun scripts/package-app.ts` (or `pnpm package:app`).
It packages for this Mac's arch; `--arch x64` builds the Intel app on Apple Silicon
(Bun cross-compiles its runtime; `KYBERN_BIN_DIR` must hold x86_64 `kybernd` and
`kybern`, e.g. `target/x86_64-apple-darwin/release`). Requires macOS, Bun
(verified 1.4.0), the installed pinned dependencies, Xcode command-line tools,
and the existing patched addon for that arch in `native/build`.
Local test builds of another arch take `--test`: the bundle becomes `Kybern GPUIX (x86_64 test).app`
with id `dev.kybern.gpuix.test`, so LaunchServices and notifications never confuse it with the
installed app (macOS 27 posts a Rosetta-retirement notice naming whatever x86_64 bundle launches).
`bun packaging/rosetta-check.ts <out>` smoke-checks that test bundle under Rosetta; it needs
macOS 27 or earlier, since macOS 28 drops Rosetta for apps. Releases never pass `--test`.
No packages are installed, no native addon is rebuilt, and nothing is installed:
no PATH change or Login Item.

Signing: with `KYBERN_CODESIGN_IDENTITY` (and `KYBERN_CODESIGN_KEYCHAIN`) set, the
dylibs, `kybernd`, `kybern` and the app are signed with that identity; otherwise
ad hoc, and the script says so. Releases sign with the self-signed "Kybern Code
Signing" certificate (no Apple Developer ID, no notarization):
`scripts/bundle-gpuix.sh` imports it from `MACOS_SIGNING_P12_BASE64` /
`MACOS_SIGNING_P12_PASSWORD` into a throwaway keychain (`scripts/macos-signing.sh`).
Every version then has the designated requirement
`identifier "dev.kybern.gpuix" and certificate leaf = H"0508616c…"` instead of a
per-build cdhash. Gatekeeper treats it as it treated the ad-hoc app (`spctl`
rejects both; the installer and updater never quarantine it). The Keychain
helper `kybern-keychain` is copied as committed and never re-signed; see
[`keychain/README.md`](keychain/README.md) for why.

Output: `dist/Kybern GPUIX.app`, already covered by this app's `dist/` ignore rule.
Bundle ID `dev.kybern.gpuix` is distinct from the retired Tauri app's `dev.kybern.desktop`.
The icons are in `packaging/icons/` (copied at build time).
No entitlements are needed for this non-sandboxed, non-hardened app.
This is a local development bundle, not notarized distribution packaging.

## Windows x64 (beta)

An unsigned, per-user NSIS installer, `kybern-gpuix_<version>_x64-setup.exe`, plus a
minisign `.sig` the in-app updater verifies. Windows GPUIX is a **beta**: it draws and
runs, but several macOS features fall back or are missing until the work packages in
[`PORTING-PLATFORMS.md`](../PORTING-PLATFORMS.md) land (see "Known gaps" below).

### Build

CI builds it in the `gpuix-windows` job of `desktop.yml` on
`windows-2022` (a job of its own, so its failure cannot skip the manifest, which publishes
the macOS assets with or without the Windows entry) (the addon through `.github/actions/gpuix-addon`, the binaries from
dist's Windows zip). By hand, in Git Bash on Windows with Bun, NSIS (`makensis` on
PATH), the patched addon and release `kybernd.exe` / `kybern.exe`:

```sh
KYBERN_BIN_DIR=/path/to/release scripts/bundle-gpuix-windows.sh   # package + sign (SKIP_SIGN=1 to skip)
cd apps/gpuix && bun scripts/package-app.ts --windows              # package only: dist/windows/
```

`package-app.ts` hands off to `scripts/package-windows.ts` on Windows or with `--windows`.
Options: `--out`, `--addon`, `--makensis`, `--no-installer` (stop after the app folder),
`--no-vc-runtime`, `--nsis-ansi`, `--webview <dll>` / `--no-webview` (the embedded web view
host, default `native/build/kybern-webview.dll` from `native/update.sh`), and
`--webview2-bootstrapper <exe>` / `--no-webview2-bootstrapper` (default: the copy cached in
the user cache folder's `kybern-gpuix-packaging`, else downloaded from Microsoft; either way it
must match the SHA-256 pinned in `windows/webview2.ts`, and on Windows its Authenticode
signature must be Microsoft Corporation's from a Microsoft Code Signing PCA. When Microsoft
replaces the file the build fails until `bun packaging/windows/webview2.ts --accept`, on
Windows, has checked the new one and printed the pin to update). It compiles `packaging/entry-windows.ts` with
`bun build --compile --target=bun-windows-x64` (a GUI-subsystem exe, no console window;
the icon and file properties are set only when built on Windows, since Bun needs Windows
APIs for them), lays out the app folder, and runs `makensis` on `windows/installer.nsi`.

A Mac or Linux machine can cross-build everything except the C++ runtime copy, to prove
the packaging compiles. Homebrew's makensis 3.13 aborts writing a Unicode installer, so
compile the check with `--nsis-ansi`:

```sh
KYBERN_BIN_DIR=<folder with stub kybernd.exe and kybern.exe> \
  bun scripts/package-app.ts --windows --nsis-ansi --out /tmp/win --addon <any file> --no-webview
```

### What the installer does

- **Where:** `%LOCALAPPDATA%\Programs\Kybern GPUIX` (no administrator prompt; a previous
  install's folder is reused; `/D=<dir>` overrides in silent mode).
- **Layout:** `Kybern GPUIX.exe`, `gpuix-native.win32-x64-msvc.node`, `kybernd.exe`, the
  app-local C++ runtime (`vcruntime140.dll`, … copied from Visual Studio's redistributable
  folder, because Rust's MSVC builds import it and a clean Windows 10 does not have it; the
  Concurrency Runtime `concrt140.dll` is not shipped, nothing here imports it), `icon.ico`,
  `version`, `release-background.png`, `THIRD_PARTY_NOTICES.md`, `src\assets\{fonts,providers}`,
  `tools\{path,shortcut,stop}.ps1`, `uninstall.exe`, and `bin\kybern.exe` with its own copy of
  the runtime DLLs (it looks beside itself first). The app finds all of it beside its own
  executable (`packaging/entry-windows.ts`). `kybern-webview.dll` is the embedded web view
  (`native/webview-win`, WebView2; WP10), and `mermaid.min.js` the Mermaid bundle it renders
  diagrams with (both left out by `--no-webview`).
- **WebView2 Runtime:** previews, interactive visuals and artifacts need the Microsoft Edge
  WebView2 Runtime. Windows 11 and most Windows 10 machines have it. The installer reads
  Microsoft's documented key (`EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}` `pv`,
  per-machine and per-user) and, only when it is missing, runs Microsoft's Evergreen bootstrapper
  (`MicrosoftEdgeWebview2Setup.exe /silent /install`, about 2 MB, carried in the installer, not
  installed). Unelevated, it installs the runtime per-user without a UAC prompt; it downloads the
  runtime, so it needs the internet. It gets at most 5 minutes (2 in a silent update). A silent
  install that fails or times out records it (`HKCU\Software\Kybern GPUIX`
  `WebView2BootstrapperFailed`) and later silent updates skip the bootstrapper; an interactive
  install always tries. A failure never fails the install: the app then opens pages in the
  browser and says how to get the runtime. The runtime is Microsoft's and updates itself.
- **Shortcuts:** a Start-menu shortcut, always, and a desktop shortcut (off by default).
  Both carry the AppUserModelID `dev.kybern.gpuix` (set by `tools\shortcut.ps1`; NSIS cannot
  set it, and a plain shortcut is the fallback), and the id is registered under
  `HKCU\Software\Classes\AppUserModelId\dev.kybern.gpuix` with the app name and icon. Toasts
  need that id: the app posts them under it (`src/native/notifications.ts`, WP5), and GPUI re-writes only the `DisplayName` value on its first toast, so the icon the installer registered stays.
- **Registry (all HKCU):** `Software\Kybern GPUIX` (`InstallDir`, `Version`, and the choices
  `PathCommand` and `DesktopShortcut`, which updates keep), the Add/Remove Programs entry
  `Software\Microsoft\Windows\CurrentVersion\Uninstall\Kybern GPUIX` (name, version, publisher,
  icon, `UninstallString`, `QuietUninstallString`, `EstimatedSize`), and the AppUserModelId key.
- **The `kybern` command:** an optional "Add the kybern command to PATH" component, on by
  default, that **appends** the install folder's `bin` to the user Path (`tools\path.ps1`, which
  edits the registry value directly so `%VAR%` entries survive, and never duplicates). Only
  `bin` goes on Path, so `uninstall`, `kybernd` and the runtime DLLs do not become commands.
  Appending is the side-by-side rule below. Settings → Terminal command is macOS-only until WP2.
- **Update mode:** `/S` is silent. `/UPDATE` starts the new app when the install finishes.
  Silent installs wait up to 30 s for `Kybern GPUIX.exe` to exit, then carry on. Only copies
  running from the install folder count (`tools\stop.ps1` checks the image path); another
  folder's Kybern GPUIX or the Tauri app's daemon is never waited for or touched. A locked
  file (a daemon that outlived the app, the loaded addon, a running `kybern`) is moved aside
  to `<name>.old` instead of killed, so running agents are not interrupted, and the next install
  or uninstall removes it. The new app restarts the old daemon through its normal version check
  (`daemon.shutdown`, `shell/daemon.ts`). A file that still cannot be written fails the install
  (exit code 2, also with `/S`) rather than leaving a half-replaced folder. If an update install
  fails, the installer starts whatever is still installed.
- **Uninstall:** `uninstall.exe` (or Add/Remove Programs) first stops the daemon running from the
  install folder (`kybern stop-daemon`, then ends what is left in that folder; this runs after
  the confirm page, so cancelling it leaves the daemon alone), then removes only the files the
  installer wrote, the shortcuts, the Path entry and the registry keys, the embedded web view's
  WebView2 data (`%LOCALAPPDATA%\kybern-gpuix\WebView2`, caches only; pages run InPrivate, and
  `kybern-gpuix` itself once empty), and the install folder. The WebView2 Runtime stays (other apps use it). Chats, settings and the
  daemon's data (`~/.kybern`) are never touched.

### Next to the Tauri Kybern app

The two apps install side by side and neither touches the other: different folders
(`%LOCALAPPDATA%\Programs\Kybern GPUIX` against the Tauri app's own), uninstall keys,
shortcuts and AppUserModelIDs (`dev.kybern.gpuix` against `dev.kybern.desktop`). They share
the user's Kybern data and local daemon, as on macOS: whichever app starts with a different
daemon version restarts it.

Both ship a `kybern.exe`. The GPUIX installer only ever **appends** its `bin` folder to the user
Path, so a `kybern` already earlier on Path (the Tauri app's, which its Settings adds the same
way, or cargo's) keeps winning and nothing is overwritten; GPUIX's copy is then reachable by
its full path. To prefer GPUIX's, move its entry up in Settings → Environment variables, or
uninstall the other. Untick the component to leave Path alone.

### Unsigned

Like the Tauri Windows build, the installer has no Authenticode signature, so a browser
download shows SmartScreen's "Windows protected your PC": choose **More info → Run anyway**.
Updates installed by the app are downloaded by the app itself, carry no Mark of the Web and are
not blocked; they are checked against the updater minisign key before they run.

### Updater

`src/platform/update.ts` reads the feed key `windows-x86_64-gpuix`, downloads the installer,
verifies the signature (`minisign.ts`), and then refuses anything that is not this release's
installer: the signature's trusted comment (`tauri signer sign` writes `file:<name>`) must name
`kybern-gpuix_<version>_x64-setup.exe`, and the version must be newer than the running app. The
updater key also signs Tauri's own setup and older GPUIX installers, which a swapped feed entry
could otherwise make the app run. A PE (`MZ`) check stays as a sanity check. The installer is
written to its own temp folder and started with `/S /UPDATE /D=<this copy's folder>` (through
`Start-Process`, because NSIS wants `/D=` last and unquoted even with spaces) and the current
environment, and the app quits.

Each environment window is its own process and keeps the install's files open, so before
that the updater asks every other window to quit over the window registry
(`windows/registry.ts`, named pipe `\\.\pipe\kybern-gpuix-…`) and waits up to 10 s. A window
that stays (stuck, or not answering) keeps running its old files; the installer moves the locked
ones aside as `.old` and the user closes that window later, after which it is the old version.

Only an installed copy updates (an `uninstall.exe` beside the exe); a loose exe or a build
output folder does not. Old staged installers are removed at the next start.
`KYBERN_UPDATE_NO_RELAUNCH=1` stages the installer and exits without running it, for checks
against a local feed.

### Requirements and known gaps

Windows 10 1809 or later (Bun's floor, and the floor of the composition clip the embedded web
view uses), x64. **WebView2 is optional**: without the runtime, previews open in the browser and
Mermaid shows as text (WP4's fallbacks); with it, they run in the app (WP10). Direct3D 11 is the renderer; the CI runner proves it only
on WARP (software). The fallbacks for everything macOS-only (no backdrop blur or Mica, fonts,
Win-key shortcuts instead of Ctrl, title-bar controls, notifications, credentials, clipboard and
`open` shell-outs) are the "macOS-only patches off macOS" table in
[`PORTING-PLATFORMS.md`](../PORTING-PLATFORMS.md); WP1 to WP5 own them.

### What CI and a real PC must still prove

See "What needs real hardware or CI" in `PORTING-PLATFORMS.md`. For the installer: the `gpuix-windows`
job runs `windows/verify-install.ps1` before the upload, and a failure blocks the job: install, files,
Add/Remove Programs, AppUserModelID read back from the shortcut, only `bin` on Path, a **smoke run**
of the installed app (`KYBERN_GPUIX_SMOKE=1`: it loads the native addon with its DLLs, imports the
whole module graph and a bundled font, then exits 0 with no window; `src/main.tsx`), a silent
update over a running `kybernd.exe` (it moves aside to `.old`, the daemon keeps running) and an
uninstall with that daemon still running (it is stopped and the folder is gone). A separate
informational step launches the installed app. It does not cover SmartScreen/Defender, a Windows 10 machine without the C++
runtime, per-monitor DPI, the real silent-update relaunch from a running app, or toasts.

## Linux packages (beta)

On Linux, `bun scripts/package-app.ts` hands over to `scripts/package-linux.ts`
(`--arch x64|arm64`, default this machine; `--formats appimage,deb,rpm`, default
all three). Releases run `scripts/bundle-gpuix-linux.sh` in the `gpuix-linux` job of
`desktop.yml` on `ubuntu-22.04` (a job of its own: a failure leaves only the
`linux-x86_64-gpuix` entry out of the manifest), which
moves the packages to the repository's `dist/` and signs the AppImage. Requires
Bun, the patched addon (`native/build/gpuix-native.linux-x64-gnu.node`, from
`native/update.sh` on Linux), `kybernd` and `kybern` for that arch in
`KYBERN_BIN_DIR` (releases use dist's static musl builds), `pnpm install` in
`apps/gpuix`, `dpkg-deb` and `dpkg-shlibdeps` (dpkg-dev), `rpmbuild` (rpm),
and for the AppImage `APPIMAGETOOL` (appimagetool 1.9.1) plus `APPIMAGE_RUNTIME`
(the type2-runtime it embeds; `desktop.yml` pins both by checksum).

The app is one folder, the same in every format:

| Path | What |
| --- | --- |
| `kybern-gpuix` | `bun build --compile --target=bun-linux-x64-baseline` (glibc, no AVX2 needed) of `packaging/entry-linux.ts` |
| `kybernd`, `kybern` | the daemon and CLI it starts and offers to link |
| `resources/` | the addon, fonts, provider icons, Mermaid, release artwork, notices, `version`, and `package` (`appimage`, `deb` or `rpm`: how `src/platform/linuxPackage.ts` knows which updater path applies) |

| Asset | Installs | Updates |
| --- | --- | --- |
| `kybern-gpuix_<v>_x86_64.AppImage` | wherever the user keeps it (the install script uses `~/Applications`); the folder sits under `usr/lib/kybern-gpuix` in the image, with `AppRun`, the desktop entry and icon at its root | in place: the app downloads the release AppImage, checks its signature, that the signature's trusted comment is exactly this release's `kybern-gpuix_<v>_x86_64.AppImage` and that the version is newer than the running app (the same check as the Windows installer; the Tauri AppImage and older GPUIX releases use the same key), that the bytes are a 64-bit x86-64 type 2 AppImage (ELF header), then writes a new file beside `$APPIMAGE` (same mode), renames it over the old one and relaunches it. If the AppImage's folder is not writable the update is announced with a download link instead |
| `kybern-gpuix_<v>_x86_64.deb` | `/usr/lib/kybern-gpuix/`, `/usr/bin/kybern-gpuix` (a link), `/usr/share/applications/dev.kybern.gpuix.desktop`, `hicolor` icons at 32, 128, 256 and 512 px | announced only: the app links the `.deb` beside the feed's AppImage and shows `sudo apt install ./kybern-gpuix_<v>_x86_64.deb` with a **Copy command** button (release details and Settings → About) |
| `kybern-gpuix_<v>_x86_64.rpm` | the same files | announced only, with `sudo dnf install ./…rpm` (`sudo zypper install --allow-unsigned-rpm ./…rpm` where zypper exists) |

**Beside the Tauri package.** The Tauri deb (package `kybern`) owns
`/usr/bin/kybern`, `/usr/bin/kybernd`, `/usr/bin/kybern-desktop`,
`kybern.desktop` and `kybern-desktop.png` icons. Kybern GPUIX (package
`kybern-gpuix`, app id `dev.kybern.gpuix`) shares none of those paths: it keeps
its daemon and CLI inside `/usr/lib/kybern-gpuix` and puts only `kybern-gpuix`
on PATH, so both install side by side without conflicts, alternatives or
`Conflicts:` fields. A terminal `kybern` comes from the Tauri package, or from
**Settings → About → Terminal command**, which links GPUIX's copy into
`~/.local/bin` (`platform/cli.ts`).

**Dependencies.** The addon links only libc, libm, libgcc_s, libxcb and
libxkbcommon(-x11); fonts go through cosmic-text, not fontconfig. It loads the
Vulkan loader, the Wayland client, wayland-egl and EGL at run time, so no ELF
tool sees those. The deb's `Depends` are `dpkg-shlibdeps` over the app
executable and the addon on the build machine plus `libvulkan1`; on
ubuntu-22.04: `libc6 (>= 2.35), libgcc-s1 (>= 4.2), libxcb1 (>= 1.12),
libxkbcommon-x11-0 (>= 0.5.0), libxkbcommon0 (>= 0.5.0), libvulkan1`, with
`Recommends: libwayland-client0, libwayland-egl1, libegl1, mesa-vulkan-drivers,
xdg-utils`. The rpm lets `rpmbuild` generate `Requires` from the same ELF files
in soname form (`libxkbcommon-x11.so.0()(64bit)`, `libc.so.6(GLIBC_2.35)(64bit)`
and so on), which resolves on Fedora and openSUSE alike although their
package names differ (on RHEL, Rocky and Alma 9 it does not: see the glibc floor below), plus `Requires: libvulkan.so.1()(64bit)` and the same
weak dependencies in soname form. On Fedora 42 that installs glibc, libgcc,
libxcb, libxkbcommon, libxkbcommon-x11 and vulkan-loader, and as weak
dependencies libwayland-client, libwayland-egl, libglvnd-egl,
mesa-vulkan-drivers and xdg-utils. The list of run-time libraries is
`RUNTIME_LIBRARIES` in `package-linux.ts`; recheck it with `strings` on the
addon after a GPUIX update. The rpm turns off stripping and every other
post-install step: the Bun executable carries the app after its ELF image, and
`strip` would cut it off (the executable is byte-identical in the AppImage, deb
and rpm).

**glibc floor: 2.35.** The packages are built on ubuntu-22.04, so the app needs
glibc 2.35 or newer: Ubuntu 22.04+, Debian 12+, Fedora 36+, openSUSE Leap 16 or Tumbleweed.
RHEL, Rocky and Alma 9 (glibc 2.34), Debian 11, Ubuntu 20.04 and openSUSE Leap 15 (2.31) are not
supported yet; the rpm's `libc.so.6(GLIBC_2.35)(64bit)` requirement fails there
inside dnf, so `install-linux-gpuix.sh` checks `getconf GNU_LIBC_VERSION` (or
`ldd --version`) first and stops with an explanation before it downloads or
installs anything. Bun's baseline x64 build avoids a second floor: the default
build needs AVX2 and dies with an illegal instruction on older CPUs and some VMs.
The addon is built without a target CPU, so it is plain x86-64.

**Install script.** `scripts/install-linux-gpuix.sh` (also `--uninstall`,
`--help`; works under `curl | sh` and is a single function run at the very end,
so a truncated download runs nothing). It refuses to install the AppImage as
root (a system-wide install is the deb or rpm; as root it never launches the
app). The AppImage is downloaded to `$DEST/.<file>.part` and renamed over
`~/Applications/Kybern-GPUIX-x86_64.AppImage`, so a running image is never
written to; `apt` gets `--allow-downgrades` (dnf `downgrade`, zypper
`--oldpackage`) only when `--version` names a release older than the installed
one. It does not verify the release's `.sig` (minisign): that needs a tool most
machines lack, and the macOS installer has no such check either; the app does
verify every update it installs. Without FUSE the menu entry is written as
`env APPIMAGE_EXTRACT_AND_RUN=1 "<image>"` (the image unpacks to a temporary
folder for each run, slower but working); with FUSE it is the plain path.
`--uninstall` removes the package or the AppImage, its menu entry and icon,
`~/.local/share/kybern-gpuix` and a `~/.local/bin/kybern` link that points into
those; it never touches `~/.kybern`.

**`kybern` on PATH, beside the Tauri app.** Neither installer script links
`~/.local/bin/kybern`; only the app's **Settings → About → Terminal command**
does, on request (`platform/cli.ts`). The rule: it never replaces a file that is
not a Kybern command or a link to one, so it won't overwrite the Tauri
AppImage's or deb's `kybern`; a link GPUIX made in `~/.local/bin` comes before
`/usr/bin` on most PATHs and so shadows the Tauri deb's command (the row says
"shadowed" and offers to remove the older copy, which deletes only another
Kybern CLI). Whichever app made its link last is the `kybern` a new terminal
runs. Both CLIs speak the same daemon protocol, and a daemon is only ever
replaced by a newer one (below).

**Which daemon runs.** The data folder `~/.kybern` is shared by every Kybern
install. The shell restarts a running daemon only when the bundled one is
newer (a higher version, a newer-built binary of the same version, or an older
protocol); an older app, such as a stale deb or an old AppImage next to an
update, leaves the newer daemon running and talks to it (`daemonPlan` in
`shell/daemon.ts`), or, when the daemon's protocol is newer than the app's,
stops with "A newer Kybern is running… update this copy". As a second guard,
`kybernd` refuses to open a store whose schema version a newer build created
(`kybern-store`), so an old binary can never run on a migrated store.

**AppImage specifics.** The image is mounted only while the app runs, but
`kybernd` outlives the app and a linked `kybern` must survive the next launch's
new mount path. `entry-linux.ts` therefore copies both to
`~/.local/share/kybern-gpuix/bin` once per version (renamed into place, so a
running daemon keeps its file), or, when that cannot be written, to
`$XDG_RUNTIME_DIR/kybern-gpuix/bin`, and runs those; the daemon restart policy
treats that copy as the packaged daemon (`KYBERN_GPUIX_PACKAGED_DAEMON`). The
daemon and CLI never run from the mount: if neither folder is writable the app
shows an error instead (`bundledCommand` skips every path inside `$APPDIR`).
The daemon is started without the app's own variables (`KYBERN_GPUIX_RESOURCES`,
the addon path, the version, `KYBERND`, `APPIMAGE`, `APPDIR`, `ARGV0`, `OWD`, and
`KYBERN_BUNDLED_CLI` when it names a path in the mount), so agents it starts see
none of them; askpass finds `kybern` beside the daemon. Extra environment windows
and the relaunch after an update start `$APPIMAGE` again instead of the mounted
executable, for the same reason, without the daemon and CLI variables so the new
process resolves its own. The type2 runtime needs `fusermount3` (or `fusermount`) but
no libfuse; without FUSE, `--appimage-extract-and-run` still works. An AppImage
unpacked with `--appimage-extract` only announces updates.

Check a build in a container: `dpkg-deb -I` / `-c`, `rpm -qip` / `-qlp` /
`-qpR`, `desktop-file-validate`, then `apt install ./…deb` on Debian/Ubuntu and
`dnf install ./…rpm` on Fedora (`which kybern-gpuix`, the desktop file and
icons present). The GUI itself needs a display; `.github/workflows/gpuix-platforms.yml`
is the on-screen check (Xvfb + lavapipe).

## Why compiled Bun

[Bun's executable documentation](https://github.com/oven-sh/bun/blob/main/docs/bundler/executables.mdx)
supports standalone compilation; [its file-type documentation](https://github.com/oven-sh/bun/blob/main/docs/runtime/file-types.mdx)
supports native addons. The dynamic NAPI override loads our existing external
`.node` from Resources. A bundled-runtime/source tree is therefore unnecessary.
The compiled app does not need Bun, node_modules, or source files beside it.

Compilation alone would break dynamic `import.meta.url` font lookups: Bun's
virtual executable filesystem is not the source tree. The build plugin rewrites
source URL bases to Resources and copies both fonts into `Resources/src/assets/fonts`.
The entry point sets the native addon path before dynamically importing the app.
It also sets the compiled app's menu name to Kybern GPUIX without editing shell
sources. The real app launched and passed the pointer workflow with this setup.

The build disables Bun's ambient dotenv, bunfig, tsconfig and package auto-loading
at executable runtime. Only the explicit runtime environment selects the daemon.
Temporary `.bun-build` files from Bun 1.4.0 are removed in the script's finally block.
Do not package over a running copy of this bundle.

## Run and checks

Run with a scratch data directory that already has a daemon:

```sh
KYBERN_DATA_DIR=/path/to/gpuix-scratch/data \
  './dist/Kybern GPUIX.app/Contents/MacOS/Kybern GPUIX'
```

The executable itself is the main macOS bundle process (not a shell launcher).
It has a Dock icon, regular activation policy, and the app name/menu. Finder can
launch the `.app`; tests execute its binary directly to reliably pass scratch
connection variables. The package does not bundle/spawn a daemon itself.

From `apps/gpuix`, these use the existing fixture to start/stop an isolated daemon
on a free port, create only scratch data, and remove it on exit:

```sh
bun src/shell/verify.fixture.ts bun packaging/pointer-check.ts
bun src/shell/verify.fixture.ts bun packaging/verify.ts bundle
bun src/shell/verify.fixture.ts bun packaging/verify.ts source
bun src/shell/verify.fixture.ts bun packaging/notification-check.ts
pnpm exec tsc --noEmit -p packaging/tsconfig.json
```

The bundle harness uses the real compiled executable with the existing stdio
automation protocol. `pointer-check.ts` mirrors the combined pointer check with
only its harness/dock-wait import locations changed.

## Notifications

macOS is described below. Windows and Linux (WP5) post through the addon instead of a companion library:
`attachNotificationHost(renderer)` (called with `setOsHost` in `main.tsx`) gives `notifications` the renderer's
`showSystemNotification` and friends. Windows shows toasts under the AppUserModelID above, Linux calls
`org.freedesktop.Notifications.Notify` with the desktop entry `dev.kybern.gpuix`. Posts carry a `group` (the thread id),
and a later post in the group replaces the earlier one; `notifications.dismiss(group)` withdraws it (the app does so
when the approval it announced is answered or the thread is looked at). Only an **installed** Windows copy (the exe
beside the installer's `uninstall.exe`) posts under `dev.kybern.gpuix`; a portable, dev or CI run posts under
`dev.kybern.gpuix.dev` (display name "Kybern (dev)"), so it never recreates the installed app's registry key or shares
its per-app toast setting. `main.tsx` hands the id to the addon (`setEarlyAppIdentity`) before the first window exists,
in every environment window's process too. See `PORTING-PLATFORMS.md`, "WP5 notifications".

`src/native/notifications.ts` exposes `status()`, `requestPermission()`,
`post({id,title,body})`, `onClick(callback)` and `reason`. Unbundled calls return
`unsupported` / `false` without invoking the abort-prone macOS notification center.
`onClick` returns an unsubscribe function. `post` never prompts implicitly.
Denied/not-yet-granted posting returns false; native service errors reject.

The small ARC Objective-C dylib uses [UNUserNotificationCenter](https://developer.apple.com/documentation/usernotifications/unusernotificationcenter)
and a strongly retained delegate. Completions and notification IDs cross a
synchronized JSON queue, woken by Bun's threadsafe callback; no polling loop.
The callback is retained for process lifetime. The native default-click handler
activates the app and makes its main window key; the JS subscriber opens the thread.
The delegate is reasserted for commands because GPUI also owns a notification
center delegate. Do not introduce a separate competing notification consumer.

An actual macOS permission prompt appeared for **Kybern GPUIX / dev.kybern.gpuix**.
The prompt was allowed through accessibility. Authorization returned `granted`,
posting returned `true`, and the real Notification Center banner was observed.
Unbundled calls were verified as `unsupported`, `unsupported`, `false` with a reason.

**Real click delivery remains unverified.** Computer-use banner clicks failed with
`noWindowsAvailable`; subsequent attempts also timed out as banners disappeared.
No `[notification click]` was observed. The delegate implementation is present,
but this is not evidence that click delivery works. Run the notification check
above, click the banner body promptly (or its entry in Notification Center), and
verify `[notification click] package-test-...` in the terminal. The test exits
after a callback or 90 seconds. On a fresh machine first choose **Allow** on the
“Kybern GPUIX” notification prompt; this machine already granted it. If denied,
turn on Allow notifications for Kybern GPUIX in System Settings → Notifications.
No permission or settings for the installed Tauri app were changed.

`agent-toasts.patch` is the exact **unapplied** shell integration patch. From the
repository root, `git apply --check apps/gpuix/packaging/agent-toasts.patch` passes.
It preserves current settings, history/deduplication/top-level-thread filters and
existing toasts; posts only while unfocused; bounds the ID mapping to 256 entries;
and only opens the thread if the original environment is still active. Native
activation precedes the navigation visually through the main queue. It includes
finished/failed, approval, and user-input notifications. Shell files were not edited.
This mapping is process-local: click routing after quitting/relaunching or after
switching environments is deliberately not implemented. Verify real click delivery
before calling notification parity complete.

## Measurements and validation (2026-10-15)

Apple M1, macOS 27, Bun 1.4.0, patched GPUIX 0.10.0, background 1280×820 Home,
isolated daemon with one idle thread, no agent running. Each process receives two
10-second warmups followed by 15-second samples. Memory is `footprint`, CPU is
process CPU-time delta, GPU is AGX accumulated time. Other desktop work and window
occlusion limit precision; these are not energy or frame-latency measurements.

Bundle allocated disk size: **87 MiB** (`du -sh`). Initial cold-ish app-ready was
3136 ms; later launches ranged 475–2332 ms. Source launch measured 1013 ms.
App-ready is the automation root appearing, before daemon hydration finishes;
these few mixed-cache launches do not prove a startup speedup.

| Launch | Sample | CPU | Physical footprint | Metal time / 15 s |
| --- | --- | ---: | ---: | ---: |
| Source | 1 | 2.53% | 162 MiB | 0.4 ms |
| Source | 2 | 2.33% | 162 MiB | 0.0 ms |
| Initial bundle | 1 | 2.60% | 99 MiB | 0.0 ms |
| Initial bundle | 2 | 2.33% | 99 MiB | 0.0 ms |
| Final bundle | 1 | 2.53% | 100 MiB | 0.0 ms |
| Final bundle | 2 | 2.40% | 100 MiB | 0.0 ms |

The compiled runtime uses less physical memory in these samples; CPU differences
are negligible. No claim is made about streaming, large transcripts or foreground GPU.

Passed: app and packaging TypeScript checking; ad-hoc deep/strict signature
verification; actual compiled app startup; Settings pointer and Space toggles;
sidebar thread selection; titlebar dock opening, entrance, Notes tab and collapse;
NSWorkspace bundle identity/name/icon/regular activation policy; AX app-menu name;
notification authorization and visible posting; unbundled safety gate.

Free space was 73 GiB before work and 72 GiB after work (other workers active). Only the final
87 MiB `.app` is retained as generated packaging output. Task-created temporary
Bun build executables were removed; all fixture daemons/data are cleaned by their
harness. Existing native builds/dependencies are untouched. No commits were made;
`feat/gpuix-mvp` was fast-forward merged at the start as requested.

## Exact unapplied shell patch

```diff
--- a/apps/gpuix/src/shell/agentToasts.ts
+++ b/apps/gpuix/src/shell/agentToasts.ts
@@ -1,4 +1,4 @@
-import { onEnvironmentChange } from './environment.ts'
+import { environmentKey, onEnvironmentChange } from './environment.ts'
 // Agent event toasts (desktop `state/rpc.ts` `announce`): a turn that finishes or
 // fails, or an agent that needs an approval or an answer, while the reader is not
 // looking at that thread. Finished and failed toasts last 6 s; approval and
@@ -11,6 +11,7 @@
 import { openThread } from '../nav/history.ts'
 import { onLiveEvent } from '../nav/live.ts'
 import { getNav } from '../nav/store.ts'
+import { notifications } from '../native/notifications.ts'
 import { isWindowVisible } from '../native/visibility.ts'
 import { getClient, getState } from '../state.ts'
 import { isWindowFocused } from './window.ts'
@@ -29,6 +30,13 @@
   const startedAt = Date.now()
   const announced = new Map<ThreadId, number>()
   const finished = new Map<ThreadId, string>()
+  const targets = new Map<string, { environment: string; thread: string }>()
+  stops.push(notifications.onClick(id => {
+    const target = targets.get(id)
+    // The native delegate activates the app/window. Never open a thread on a
+    // different daemon after an environment switch.
+    if (target?.environment === environmentKey()) openThread(target.thread)
+  }))
   let enabled = true
 
   const readSettings = (settings: Settings | undefined) => {
@@ -67,6 +75,19 @@
       if (event.turn_id) finished.set(event.thread_id, event.turn_id)
     }
     const title = thread.title || 'Thread'
+    if (!isWindowFocused()) {
+      const id = `agent:${event.thread_id}:${event.seq}`
+      targets.set(id, { environment: environmentKey(), thread: event.thread_id })
+      if (targets.size > 256) targets.delete(targets.keys().next().value!)
+      const body = event.kind === 'turn_failed' ? `Failed: ${event.error}`
+        : event.kind === 'approval_requested' ? `Needs approval: ${event.approval.summary}`
+        : event.kind === 'user_input_requested' ? 'Needs your input' : 'Finished working'
+      void notifications.requestPermission().then(status => {
+        if (status === 'granted' && getClient() === client && enabled && !isWindowFocused()) {
+          return notifications.post({ id, title, body })
+        }
+      }).catch(error => console.error('[notifications]', error))
+    }
     const open = { label: 'Open thread', onClick: () => openThread(event.thread_id) }
     if (event.kind === 'user_input_requested' || event.kind === 'approval_requested') {
       toast(title, {
```
