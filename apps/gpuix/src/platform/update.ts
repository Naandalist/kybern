// App self-update from the release feed and minisign public key in
// `updater.json` (the ones the retired Tauri updater used): checked 15 s after
// launch and every 6 h, announced once per version, installed on request with
// download progress, then relaunched.
// The packaged daemon is restarted by the shell on the next launch because its
// version no longer matches (`shell/daemon.ts`).
//
// What Tauri's updater did natively is done here: fetch `latest.json`, pick
// this build's platform entry, download it, verify the minisign signature
// (`minisign.ts`), unpack the `.app.tar.gz` and swap the bundle in place.
// GPUIX ships as its own bundle (`dev.kybern.gpuix`), so its feed entry is
// `darwin-aarch64-gpuix` (Tauri's `{target}-{bundle}` key form). Only the
// `-gpuix` keys are read; plain keys from older feeds are ignored.
//
// Windows (beta) is a per-user NSIS install (`packaging/windows/installer.nsi`),
// feed key `windows-x86_64-gpuix`. The signed asset is the installer itself: it
// is downloaded, verified, started silently (`/S /UPDATE`) and this process
// quits. Before that the other GPUIX window processes are asked to quit (each
// holds the install's files open; `windows/registry.ts`), and the installer is
// told the folder of this copy (`/D=`). The installer waits for the app to
// exit, replaces the files (a locked daemon is moved aside, not killed) and
// starts the new app with this process's environment. The signature must name
// this release's installer, so a validly signed Tauri setup or an older GPUIX
// installer in the feed is refused. Only an installed copy updates; a loose
// `Kybern GPUIX.exe` has no `uninstall.exe` beside it and stays put.
//
// Linux (`linux-x86_64-gpuix`, the AppImage): a running AppImage replaces its
// own file and relaunches it. Its signature must name exactly this release's
// AppImage (`kybern-gpuix_<version>_x86_64.AppImage`) and a newer version, the
// same check as the Windows installer (`assertSignedRelease`), and the download
// must be an x86-64 type 2 AppImage before it is swapped in. An AppImage whose
// folder the user cannot write, a deb or rpm install, and an unpacked AppImage
// only announce the release with a download link (a package's install command
// too; `linuxPackage.ts` reads which format this is).
//
// Update tracks: the local daemon's `update_track` setting picks the feed.
// Stable reads `latest.json` of the latest GitHub release. Dev also reads
// `latest-dev.json` on the rolling `dev-feed` prerelease, which every
// `vX.Y.Z-dev.N` release replaces, and takes the newer of the two, so a stable
// release that overtakes the dev line reaches Dev as well. Versions compare as
// semver, so switching Dev → Stable never offers a downgrade: the dev build
// stays until a stable release is newer. The last track a local daemon reported
// is remembered for checks made while it is unreachable or while this window
// is at another machine.
//
// Updates this machine was asked for (ADE-125: "Update all machines", "Keep all
// machines on the same version"): each window of the local daemon attaches as
// its updater (`app_updater.attach`). Once nothing runs, the daemon sends one
// window `app_update.requested`; it installs the newest release of its own track
// with the steps above, without asking, waits again until no turn, agent or
// queued message is running (and, for an update that follows another machine,
// no terminal), shows a short notice and restarts (the new app restarts the
// daemon). A window someone used in the last two minutes asks first (Restart
// now / Later); Later waits until it has been left alone for five minutes. It
// tells the daemon how far it got (`app_updater.report`).
// While the app is closed the daemon updates it itself.
//
// Tests point the app at a local feed with KYBERN_UPDATE_FEED and
// KYBERN_UPDATE_DEV_FEED (loopback only) and KYBERN_UPDATE_PUBKEY; nothing
// else can change the key.
import { spawn, execFile } from 'node:child_process'
import { existsSync } from 'node:fs'
import { chmod, mkdtemp, open, readdir, rename, rm, stat, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { promisify } from 'node:util'
import { useSyncExternalStore } from 'react'
import updater from './updater.json'
import { desktopVersion, relaunchEnv } from '../shell/daemon.ts'
import { compareVersions } from '../shell/platform.ts'
import { readPref, writePref } from './prefs.ts'
import { verifyMinisign } from './minisign.ts'
import { installerName } from '../../packaging/windows/lib.ts'
import { quitOtherWindows } from '../windows/registry.ts'
import { appImageName, imageDirWritable, linuxInstall, packageCommand } from './linuxPackage.ts'
import { installedWindowsFolder } from './windowsInstall.ts'
import type { KybernClient } from '../../../../packages/kybern-client/src/client.ts'
import { APP_UPDATE_REQUESTED_NOTIFICATION, type AppUpdateRequestedNotification, type AppUpdaterReportParams, type DaemonActivity } from '../../../../packages/kybern-client/src/types.ts'
import { sinceUserInput } from './userInput.ts'

const run = promisify(execFile)

export type AppUpdatePhase = 'idle' | 'checking' | 'current' | 'available' | 'installing' | 'error'
export interface AppUpdateState {
  phase: AppUpdatePhase
  appVersion: string | null
  version: string | null
  notes: string | null
  /** 0–1 while installing; null until the size is known. */
  progress: number | null
  error: string | null
  errorKind: 'check' | 'install' | null
  checkedAt: number | null
  announcementOpen: boolean
  detailsOpen: boolean
  /** A newer release whose feed has no GPUIX build yet (not installable). */
  unpublished: string | null
  /** For a deb or rpm install (or an unpacked AppImage): where to download the release, and the command that installs it. */
  manual: ManualUpdate | null
}
export interface ManualUpdate { url: string; file: string; command: string | null }
export type UpdateTrack = 'stable' | 'dev'

let state: AppUpdateState = { phase: 'idle', appVersion: null, version: null, notes: null, progress: null, error: null, errorKind: null, checkedAt: null, announcementOpen: false, detailsOpen: false, unpublished: null, manual: null }
const listeners = new Set<() => void>()
const set = (patch: Partial<AppUpdateState>) => { state = { ...state, ...patch }; for (const l of listeners) l() }
export const getAppUpdate = () => state
export function useAppUpdate<T>(select: (s: AppUpdateState) => T): T {
  return useSyncExternalStore((l) => { listeners.add(l); return () => { listeners.delete(l) } }, () => select(state))
}

const FIRST_CHECK_DELAY = 15_000
const CHECK_INTERVAL = 6 * 60 * 60 * 1000
const DISMISSED_UPDATE_KEY = 'kybern.update.dismissed-version'
const TRACK_KEY = 'kybern.update.track'
/** The feed entry for this build (Tauri's `{os}-{arch}-{bundle}` form). Windows is x64 only. */
export const platformKey = (platform: NodeJS.Platform, arch: string) =>
  platform === 'win32' ? 'windows-x86_64-gpuix' : `${platform === 'linux' ? 'linux' : 'darwin'}-${arch === 'arm64' ? 'aarch64' : 'x86_64'}-gpuix`
export const PLATFORM_KEY = platformKey(process.platform, process.arch)

interface FeedEntry { url: string; signature: string }
interface Pending { version: string; notes: string | null; entry: FeedEntry }
let pending: Pending | null = null
let timer: ReturnType<typeof setTimeout> | null = null
let checkInFlight: Promise<void> | null = null
let installInFlight: Promise<void> | null = null
let dismissedInSession: string | null = null

/** The `.app` this process runs from, or null outside a packaged build. */
export function appBundle(execPath = process.execPath): string | null {
  // Checks against a local feed may name a scratch bundle to replace.
  if (process.env.KYBERN_UPDATE_BUNDLE && testFeed()) return process.env.KYBERN_UPDATE_BUNDLE
  const match = /^(.*\.app)\/Contents\/MacOS\/[^/]+$/.exec(execPath)
  return match ? match[1]! : null
}

/**
 * The install folder this process runs from on Windows, or null for anything
 * else: a loose copy, `bun`, a build output folder. The installer writes
 * `uninstall.exe` beside the app, which is what marks an installed copy.
 */
export function windowsInstall(execPath = process.execPath, exists: (file: string) => boolean = existsSync): string | null {
  // Checks against a local feed may name a scratch folder to treat as the install.
  if (process.env.KYBERN_UPDATE_BUNDLE && testFeed()) return process.env.KYBERN_UPDATE_BUNDLE
  return installedWindowsFolder(execPath, exists)
}
function testFeed(name: 'KYBERN_UPDATE_FEED' | 'KYBERN_UPDATE_DEV_FEED' = 'KYBERN_UPDATE_FEED'): URL | null {
  const override = process.env[name]
  if (!override) return null
  const url = new URL(override)
  if (url.protocol !== 'http:' || !['127.0.0.1', 'localhost', '[::1]'].includes(url.hostname)) throw Error(`${name} must be a loopback http address`)
  return url
}

export interface Feeds { stable: string[]; dev: string[] }
function feed(): Feeds & { pubkey: string } {
  const local = testFeed()
  if (local) return { stable: [local.href], dev: [testFeed('KYBERN_UPDATE_DEV_FEED')?.href].filter((href): href is string => !!href), pubkey: process.env.KYBERN_UPDATE_PUBKEY ?? updater.pubkey }
  return { stable: updater.endpoints, dev: updater.devEndpoints, pubkey: updater.pubkey }
}

/** True when this build can replace itself: a packaged `.app` on macOS, an installed copy on Windows, a running AppImage in a folder its user can write. */
export const canSelfUpdate = (): boolean => process.env.KYBERN_DISABLE_UPDATES !== '1' &&
  (process.platform === 'darwin' ? !!appBundle() : process.platform === 'win32' ? !!windowsInstall() : appImageUpdatable())
function appImageUpdatable(install = linuxInstall(), writable = imageDirWritable) {
  return install?.kind === 'appimage' && writable(install.image)
}
/** True when this build looks for releases: every packaged build. deb and rpm installs only announce them. */
export const canCheckForUpdates = (): boolean => canSelfUpdate() || (process.env.KYBERN_DISABLE_UPDATES !== '1' && linuxInstall() !== null)

/**
 * What a build that cannot replace itself offers instead. The feed carries only
 * the AppImage; the deb and rpm are its release siblings with the same name stem.
 */
export function manualUpdate(entry: FeedEntry, install = linuxInstall(), writable = imageDirWritable): ManualUpdate | null {
  if (!install || (install.kind === 'appimage' && writable(install.image))) return null
  const kind = install.kind === 'unpacked' || install.kind === 'appimage' ? null : install.kind
  const url = kind ? entry.url.replace(/\.AppImage$/, `.${kind}`) : entry.url
  const file = decodeURIComponent(url.slice(url.lastIndexOf('/') + 1))
  return { url, file, command: kind && url !== entry.url ? packageCommand(kind, file) : null }
}

export function loadAppVersion() {
  try { set({ appVersion: desktopVersion() }) } catch { /* About shows an ellipsis instead. */ }
}

/** What to do with a release this build cannot install itself. */
export const manualUpdateText = (manual: ManualUpdate) => manual.command
  ? `Download the package, then run ${manual.command} in the folder it saved to.`
  : 'Download the new AppImage and replace this copy with it.'

/** Ask the release feed for a newer version. */
export function checkForAppUpdate(options: { manual?: boolean } = {}): Promise<void> {
  if (!canCheckForUpdates()) return Promise.resolve()
  if (installInFlight) return installInFlight
  if (checkInFlight) return checkInFlight
  checkInFlight = runUpdateCheck(options).finally(() => { checkInFlight = null })
  return checkInFlight
}

type FeedRelease = { version: string; notes: string | null; entry: FeedEntry | null }

/** The release one feed announces, from the first endpoint that answers; null when it answers 204. */
async function readManifest(endpoints: string[], fetcher: typeof fetch): Promise<FeedRelease | null> {
  let lastError: unknown
  for (const endpoint of endpoints) {
    try {
      const response = await fetcher(endpoint, { signal: AbortSignal.timeout(30_000), headers: { Accept: 'application/json' } })
      if (response.status === 204) return null
      if (!response.ok) throw Error(`release JSON request failed (${response.status})`)
      const data = await response.json() as { version?: unknown; notes?: unknown; platforms?: Record<string, Partial<FeedEntry>> }
      if (typeof data.version !== 'string') throw Error('The update manifest is incomplete. Try again later.')
      const version = data.version.replace(/^v/, '')
      const raw = data.platforms?.[PLATFORM_KEY]
      const entry = raw && typeof raw.url === 'string' && typeof raw.signature === 'string' ? { url: raw.url, signature: raw.signature } : null
      return { version, notes: typeof data.notes === 'string' ? data.notes : null, entry }
    } catch (error) { lastError = error }
  }
  throw lastError
}

/** The feed's release when it is newer than `current`. */
export async function readFeed(current: string, endpoints: string[], fetcher: typeof fetch = fetch): Promise<FeedRelease | null> {
  const release = await readManifest(endpoints, fetcher)
  return release && compareVersions(release.version, current) > 0 ? release : null
}

/**
 * The newest release `track` offers that is newer than `current`. Stable reads only its own feed. Dev reads both
 * and takes the newer, so a stable release that overtakes the dev line arrives on Dev too; one feed failing (the dev
 * feed is missing until the first dev release) only matters when the other fails as well.
 */
export async function readTrack(current: string, track: UpdateTrack, feeds: Feeds, fetcher: typeof fetch = fetch): Promise<FeedRelease | null> {
  if (track === 'stable' || !feeds.dev.length) return readFeed(current, feeds.stable, fetcher)
  const results = await Promise.allSettled([readManifest(feeds.stable, fetcher), readManifest(feeds.dev, fetcher)])
  const found = results.flatMap(result => result.status === 'fulfilled' && result.value ? [result.value] : [])
  if (!found.length) {
    const failed = results.find((result): result is PromiseRejectedResult => result.status === 'rejected')
    if (failed) throw failed.reason
    return null
  }
  const newest = found.reduce((a, b) => compareVersions(b.version, a.version) > 0 ? b : a)
  return compareVersions(newest.version, current) > 0 ? newest : null
}

const storedTrack = (): UpdateTrack => readPref<UpdateTrack>(TRACK_KEY, 'stable') === 'dev' ? 'dev' : 'stable'
function rememberTrack(track: UpdateTrack) {
  try { if (storedTrack() !== track) writePref(TRACK_KEY, track) } catch { /* the next check asks the daemon again */ }
}

/** The track this app follows: the local daemon's `update_track`, or the last one it reported. */
async function currentTrack(): Promise<UpdateTrack> {
  try {
    const [{ activeEnvironment }, { getClient }] = await Promise.all([import('../shell/environment.ts'), import('../state.ts')])
    const client = activeEnvironment().local ? getClient() : null
    if (client) rememberTrack((await client.call('settings.get', {})).update_track === 'dev' ? 'dev' : 'stable')
  } catch { /* the remembered track stands */ }
  return storedTrack()
}

/** After the update track changed in Settings: remember it and look again, so Dev sees the dev build at once and Stable drops an offer it no longer takes. */
export function updateTrackChanged(track: UpdateTrack) {
  rememberTrack(track)
  if (state.phase === 'installing') return
  void (checkInFlight ?? Promise.resolve()).then(() => checkForAppUpdate())
}

async function runUpdateCheck(options: { manual?: boolean }) {
  if (state.phase === 'installing') return
  set({ phase: 'checking', error: null, errorKind: null })
  try {
    const result = await readTrack(desktopVersion(), await currentTrack(), feed())
    if (!result || !result.entry) {
      pending = null
      set({ phase: 'current', version: null, notes: null, checkedAt: Date.now(), announcementOpen: false, detailsOpen: false, unpublished: result?.version ?? null, manual: null })
      return
    }
    pending = { version: result.version, notes: result.notes, entry: result.entry }
    set({ phase: 'available', version: result.version, notes: result.notes, checkedAt: Date.now(), unpublished: null, manual: manualUpdate(result.entry), announcementOpen: dismissedUpdateVersion() !== result.version })
  } catch (error) {
    set({ phase: pending ? 'available' : 'error', error: describe(error), errorKind: 'check', checkedAt: Date.now() })
    if (options.manual) void import('../shell/toasts.tsx').then(({ toast }) => toast.error('Unable to check for updates', { description: describe(error) }))
  }
}

/** Download, verify and install the pending update, then relaunch. A deb or rpm install opens the package's download instead. */
export function installAppUpdate(): Promise<void> {
  if (installInFlight) return installInFlight
  if (state.phase === 'installing') return Promise.resolve()
  if (checkInFlight) return checkInFlight.then(() => installAppUpdate())
  const update = pending
  if (!update) return Promise.resolve()
  if (state.manual) return import('../pulls/PrStates.tsx').then(({ openUrl }) => openUrl(state.manual!.url))
  installInFlight = runUpdateInstall(update).finally(() => { installInFlight = null })
  return installInFlight
}

/** Stream the download, reporting progress like Tauri's Started/Progress/Finished events. */
export async function download(url: string, onProgress: (progress: number | null) => void, fetcher: typeof fetch = fetch): Promise<Uint8Array> {
  const response = await fetcher(url, { signal: AbortSignal.timeout(10 * 60_000) })
  if (!response.ok || !response.body) throw Error(`Download request failed with status: ${response.status}`)
  const length = Number(response.headers.get('content-length'))
  const total = Number.isFinite(length) && length > 0 ? length : null
  onProgress(total === null ? null : 0)
  const chunks: Uint8Array[] = []
  let received = 0
  for await (const chunk of response.body as unknown as AsyncIterable<Uint8Array>) {
    chunks.push(chunk)
    received += chunk.length
    if (total !== null) onProgress(Math.max(0, Math.min(1, received / total)))
  }
  onProgress(1)
  return Buffer.concat(chunks)
}

/** What the `.app` in a macOS update must be: the release the feed offered, newer than `current`, on `track`. */
export interface BundleExpectation { version: string; current: string; track: UpdateTrack }

/**
 * The macOS archive's signature names the same file in every release, so the unpacked bundle's own version
 * (`CFBundleShortVersionString`, set by `scripts/package-app.ts`) must be the release's, newer than this one, and
 * one this machine's track takes (Stable never takes a prerelease). An older signed archive is refused.
 */
export function assertBundleVersion(found: string, expected: BundleExpectation) {
  const version = found.trim()
  if (version !== expected.version) throw Error(`The update contains Kybern ${version || 'of an unknown version'}, not ${expected.version}. Download it from the release page.`)
  if (compareVersions(version, expected.current) <= 0) throw Error('The update is not newer than this version. Download it from the release page.')
  if (expected.track === 'stable' && version.includes('-')) throw Error('The update is a Dev build, and this machine takes Stable releases. Download it from the release page.')
}

/**
 * Replace `bundle` with the `.app` inside a verified `.app.tar.gz`. Everything
 * is staged beside the bundle (same volume, so both renames are atomic) and the
 * old bundle comes back if the swap fails. The new bundle must keep this app's
 * identifier, so a mislabelled artifact can never replace Kybern GPUIX, and be
 * the expected release (`assertBundleVersion`), so an older one never does either.
 */
export async function replaceBundle(bundle: string, archive: Uint8Array, expected: BundleExpectation, identifier = 'dev.kybern.gpuix') {
  const parent = path.dirname(bundle)
  const staging = await mkdtemp(path.join(parent, '.kybern-update-'))
  try {
    const file = path.join(staging, 'update.app.tar.gz')
    await writeFile(file, archive)
    const unpacked = path.join(staging, 'unpacked')
    await run('/bin/mkdir', [unpacked])
    await run('/usr/bin/tar', ['-xzf', file, '-C', unpacked])
    const apps = (await readdir(unpacked)).filter(name => name.endsWith('.app'))
    if (apps.length !== 1) throw Error('The update archive does not contain one app')
    const next = path.join(unpacked, apps[0]!)
    const { stdout } = await run('/usr/libexec/PlistBuddy', ['-c', 'Print :CFBundleIdentifier', path.join(next, 'Contents/Info.plist')])
    if (stdout.trim() !== identifier) throw Error(`The update is for ${stdout.trim() || 'another app'}, not this one`)
    const found = await run('/usr/libexec/PlistBuddy', ['-c', 'Print :CFBundleShortVersionString', path.join(next, 'Contents/Info.plist')]).then(({ stdout }) => stdout, () => '')
    assertBundleVersion(found, expected)
    const backup = path.join(staging, 'previous.app')
    await rename(bundle, backup)
    try { await rename(next, bundle) } catch (error) { await rename(backup, bundle); throw error }
  } finally {
    await rm(staging, { recursive: true, force: true })
  }
}

/** ELF `e_machine` of the architectures the feed carries a GPUIX AppImage for. */
const ELF_MACHINE: Record<string, number> = { x64: 62, arm64: 183 }

/**
 * Replace the running AppImage `image` with a verified download. The new file
 * is written beside it (created exclusively, with the old file's mode) and
 * renamed over it, so the mounted old image keeps running until the relaunch.
 * `signedName` is the file name the release signature covers and must be exactly
 * this `version`'s AppImage: Tauri's AppImage and older GPUIX releases are signed
 * with the same key, so a mislabelled feed entry must not replace Kybern GPUIX
 * with them. The bytes must be a type 2 AppImage for this machine's architecture.
 */
export async function replaceAppImage(image: string, data: Uint8Array, signedName: string, version: string, arch: string = process.arch) {
  if (signedName !== appImageName(version, arch)) throw Error(`The update is ${signedName || 'another file'}, not a Kybern GPUIX ${version} AppImage`)
  // A type 2 AppImage is an ELF runtime with the magic bytes `AI\x02` at offset 8. Byte 4 is the class (2 = 64-bit),
  // byte 5 the data encoding (1 = little endian) and bytes 18–19 `e_machine` (62 = x86-64, 183 = AArch64).
  const elf = data[0] === 0x7f && data[1] === 0x45 && data[2] === 0x4c && data[3] === 0x46
  if (!elf || data[8] !== 0x41 || data[9] !== 0x49 || data[10] !== 0x02) throw Error('The update is not an AppImage')
  if (data[4] !== 2 || data[5] !== 1 || data.length < 20 || (data[18]! | (data[19]! << 8)) !== ELF_MACHINE[arch]) throw Error(`The update is not an AppImage for ${arch === 'arm64' ? 'arm64' : 'x86-64'}`)
  const next = path.join(path.dirname(image), `.${path.basename(image)}.update-${process.pid}`)
  let created = false
  try {
    const mode = ((await stat(image)).mode & 0o777) | 0o100
    const handle = await open(next, 'wx', mode)
    created = true
    try { await handle.writeFile(data) } finally { await handle.close() }
    await chmod(next, mode)
    await rename(next, image)
  } catch (error) {
    if (created) await rm(next, { force: true })
    if ((error as NodeJS.ErrnoException).code === 'EACCES' || (error as NodeJS.ErrnoException).code === 'EROFS') throw Error(`Kybern can’t write to ${path.dirname(image)}. Move the AppImage to a folder you own, or download the update from the release page.`)
    throw error
  }
}

/**
 * Download and verify `update` for this install. Nothing is replaced yet: the returned step swaps it in and restarts
 * the app, which ends this process.
 */
async function prepareInstall(update: Pending, onProgress: (progress: number | null) => void): Promise<() => Promise<void>> {
  const install = process.platform === 'linux' ? linuxInstall() : null
  if (install?.kind === 'appimage') {
    const data = await download(update.entry.url, onProgress)
    const { trustedComment } = verifyMinisign(data, update.entry.signature, feed().pubkey)
    assertSignedRelease(trustedComment, appImageName(update.version), update.version, desktopVersion())
    return async () => {
      await replaceAppImage(install.image, data, signedFile(trustedComment) ?? '', update.version)
      await relaunchAppImage(install.image)
    }
  }
  const windows = process.platform === 'win32'
  const bundle = windows ? windowsInstall() : appBundle()
  if (!bundle) throw Error('Only the packaged app can update itself')
  const archive = await download(update.entry.url, onProgress)
  const { trustedComment } = verifyMinisign(archive, update.entry.signature, feed().pubkey)
  if (windows) {
    assertWindowsInstaller(trustedComment, update.version, desktopVersion())
    return () => runWindowsInstaller(archive, bundle)
  }
  const expected: BundleExpectation = { version: update.version, current: desktopVersion(), track: await currentTrack() }
  return async () => {
    await replaceBundle(bundle, archive, expected)
    await relaunch(bundle)
  }
}

async function runUpdateInstall(update: Pending) {
  set({ phase: 'installing', progress: null, error: null, errorKind: null })
  try {
    const apply = await prepareInstall(update, (progress) => set({ progress }))
    await apply()
  } catch (error) {
    set({ phase: 'available', progress: null, error: describe(error), errorKind: 'install' })
    void import('../shell/toasts.tsx').then(({ toast }) => toast.error('Unable to install the update', { description: describe(error) }))
  }
}

/** The `file:` field of a `tauri signer sign` trusted comment (`timestamp:<unix>\tfile:<name>`). */
export const signedFile = (trustedComment: string) => trustedComment.split('\t').find(field => field.startsWith('file:'))?.slice('file:'.length) ?? null

/**
 * The trusted comment is covered by the signature, so it binds the download to a file name. The updater key also
 * signs Tauri's own `-setup.exe` and `.AppImage` and older GPUIX releases, so a swapped feed entry would otherwise
 * install a validly signed build of the wrong app or version. Windows and the Linux AppImage check it; the macOS
 * archive is checked by its bundle identifier and version instead (`replaceBundle`, `assertBundleVersion`).
 */
export function assertSignedRelease(trustedComment: string, expected: string, version: string, current: string) {
  if (signedFile(trustedComment) !== expected) throw Error('The update is not the build for this release. Download it from the release page.')
  if (compareVersions(version, current) <= 0) throw Error('The update is not newer than this version. Download it from the release page.')
}
export const assertWindowsInstaller = (trustedComment: string, version: string, current: string) =>
  assertSignedRelease(trustedComment, installerName(version), version, current)

const INSTALLER_DIR_PREFIX = 'kybern-gpuix-update-'

/** Write a verified installer to its own scratch folder; the installer cannot delete itself while it runs. */
export async function stageWindowsInstaller(installer: Uint8Array, parent = tmpdir()): Promise<string> {
  // Every PE file starts with "MZ"; a feed entry that points at something else is refused before it runs.
  if (installer[0] !== 0x4d || installer[1] !== 0x5a) throw Error('The update is not a Windows installer')
  const dir = await mkdtemp(path.join(parent, INSTALLER_DIR_PREFIX))
  const file = path.join(dir, 'kybern-gpuix-setup.exe')
  await writeFile(file, installer)
  return file
}

/** Installers from earlier updates (they cannot remove themselves); best effort. */
export async function removeStaleInstallers(parent = tmpdir(), now = Date.now()) {
  for (const name of await readdir(parent).catch(() => [] as string[])) {
    if (!name.startsWith(INSTALLER_DIR_PREFIX)) continue
    const dir = path.join(parent, name)
    const modified = await stat(dir).then(value => value.mtimeMs, () => now)
    if (now - modified > 60 * 60 * 1000) await rm(dir, { recursive: true, force: true }).catch(() => {})
  }
}

/**
 * The PowerShell command that starts the installer. NSIS wants `/D=<dir>` last, unquoted, even when the folder
 * has spaces, and reads the program name from a quoted first token; a Node or Bun argument array quotes
 * the argument instead. `Start-Process` takes one argument string and passes it through as written.
 */
export function windowsInstallerCommand(file: string, installDir: string, systemRoot = process.env.SystemRoot ?? 'C:\\Windows') {
  const quote = (value: string) => `'${value.replaceAll("'", "''")}'`
  const dir = /^[A-Za-z]:\\$/.test(installDir) ? installDir : installDir.replace(/\\+$/, '')
  return {
    command: path.win32.join(systemRoot, 'System32', 'WindowsPowerShell', 'v1.0', 'powershell.exe'),
    args: ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-Command', `Start-Process -FilePath ${quote(file)} -ArgumentList ${quote(`/S /UPDATE /D=${dir}`)}`],
  }
}

/**
 * Run the installer silently and quit. `/UPDATE` makes it start the new app
 * itself when it finishes (this process is gone by then); the environment is
 * handed over so the same daemon and data folder are used. The other windows
 * are asked to quit first; any that stay (the wait is 10 s) keep running the
 * old files, which the installer moves aside as `.old`, until they are closed.
 */
async function runWindowsInstaller(installer: Uint8Array, installDir: string) {
  const file = await stageWindowsInstaller(installer)
  const stayed = await quitOtherWindows().catch(() => [] as number[])
  if (stayed.length) console.error('[update] other Kybern GPUIX windows are still open:', stayed.join(', '))
  if (process.env.KYBERN_UPDATE_NO_RELAUNCH === '1') { console.error('[update] staged', file); process.exit(0) }
  const { command, args } = windowsInstallerCommand(file, installDir)
  await run(command, args, { windowsHide: true, env: relaunchEnv(process.env) })
  process.exit(0)
}

/**
 * Start the new bundle's executable with this process's environment, then quit
 * (Tauri's `relaunch`, which spawns the binary directly). Keeping the
 * environment keeps the same daemon; its bundled copy restarts once its version
 * no longer matches the app's.
 */
async function relaunch(bundle: string) {
  if (process.env.KYBERN_UPDATE_NO_RELAUNCH === '1') { console.error('[update] installed', bundle); process.exit(0) }
  const { stdout } = await run('/usr/libexec/PlistBuddy', ['-c', 'Print :CFBundleExecutable', path.join(bundle, 'Contents/Info.plist')])
  const child = spawn(path.join(bundle, 'Contents/MacOS', stdout.trim()), [], { detached: true, stdio: 'ignore', env: relaunchEnv(process.env) })
  await new Promise<void>((resolve, reject) => { child.once('spawn', resolve); child.once('error', reject) })
  child.unref()
  process.exit(0)
}

/**
 * Start the replaced AppImage, then quit. It mounts afresh and sets its own
 * APPIMAGE and APPDIR; the rest of the environment is kept, as on macOS, except
 * the daemon and CLI this process resolved, which the new image resolves for itself.
 */
async function relaunchAppImage(image: string) {
  if (process.env.KYBERN_UPDATE_NO_RELAUNCH === '1') { console.error('[update] installed', image); process.exit(0) }
  const env = relaunchEnv(process.env)
  const child = spawn(image, [], { detached: true, stdio: 'ignore', env })
  await new Promise<void>((resolve, reject) => { child.once('spawn', resolve); child.once('error', reject) })
  child.unref()
  process.exit(0)
}

// ---- updates this machine was asked for (ADE-125) ----

/** How often a verified update looks again whether work finished. */
export const IDLE_POLL_MS = 5000
/** How long the notice shows before the app restarts. */
export const RESTART_NOTICE_MS = 2500
/** Input this recent means someone is using the window: it asks before restarting. */
export const IN_USE_MS = 2 * 60_000
/** After Later (or while the question is unanswered), how long the window must be left alone before it restarts. */
export const LATER_QUIET_MS = 5 * 60_000
const RESTART_TOAST = 'kybern-update-restart'

/**
 * Nothing a restart would interrupt: no turn or approval, no agent or background work, no queued message. An
 * `automatic` update (following another machine) also waits for terminals, whose processes a restart ends.
 */
export const activityIdle = (activity: Pick<DaemonActivity, 'running_threads' | 'live_sessions' | 'idle_sessions' | 'queued_messages'> & Partial<Pick<DaemonActivity, 'terminals'>>, automatic = false) =>
  activity.running_threads === 0 && activity.live_sessions === activity.idle_sessions && activity.queued_messages === 0 && (!automatic || !activity.terminals)

/** The question a window in use is asked before it restarts. `hide` takes it away without answering. */
export interface RestartPrompt { choice: Promise<'now' | 'later'>; hide: () => void }

/** What a requested update needs; the app's own updater and daemon in use, a test's in tests. */
export interface RequestedUpdateDeps {
  /** The newest release of this app's update track, newer than this version; null when there is none. */
  check: () => Promise<Pending | null>
  /** Download and verify it; the step it returns installs it and restarts. */
  prepare: (update: Pending) => Promise<() => Promise<void>>
  /** Whether nothing is running on this machine's daemon. */
  idle: () => Promise<boolean>
  report: (params: AppUpdaterReportParams) => Promise<void>
  /** Show the notice before the restart; the returned function takes it away again. */
  notice: (version: string) => Promise<() => void>
  /** Milliseconds since this window last had the user's input. */
  sinceInput: () => number
  /** Ask whether to restart now or later; closing the question counts as Later. */
  ask: (version: string) => RestartPrompt
  sleep: (ms: number) => Promise<void>
}
export type RequestedOutcome = 'current' | 'failed' | 'restarted' | 'busy'

/**
 * The daemon asked for an update. Install the newest release of this app's track, verified as always, then restart
 * once nothing is running, and never in the middle of a turn: the machine is checked again after the notice shows.
 * A window someone is using asks first: Restart now restarts as soon as nothing runs; Later (or no answer) waits
 * until the window has been left alone for `LATER_QUIET_MS`, then restarts after the usual notice.
 */
export async function runRequestedUpdate(deps: RequestedUpdateDeps): Promise<RequestedOutcome> {
  const fail = async (error: unknown) => { await deps.report({ phase: 'failed', message: describe(error) }); return 'failed' as const }
  let update: Pending | null
  try { update = await deps.check() } catch (error) { return fail(error) }
  if (!update) { await deps.report({ phase: 'current' }); return 'current' }
  const version = update.version
  await deps.report({ phase: 'installing', version })
  let apply: () => Promise<void>
  try { apply = await deps.prepare(update) } catch (error) { return fail(error) }
  let waiting = false
  let prompt: RestartPrompt | null = null
  let answer: 'now' | 'later' | null = null
  let consented = false
  let deferred = false
  for (;;) {
    if (answer === 'now') { consented = true; prompt = null; answer = null }
    if (answer === 'later') { deferred = true; prompt = null; answer = null }
    if (await deps.idle()) {
      if (consented) break
      const asked = prompt !== null || deferred
      if (deps.sinceInput() >= (asked ? LATER_QUIET_MS : IN_USE_MS)) {
        // Nobody is using the window: an unanswered question goes, and the notice shows before the restart.
        const question: RestartPrompt | null = prompt
        prompt = null
        question?.hide()
        const hide = await deps.notice(version)
        await deps.sleep(RESTART_NOTICE_MS)
        if (await deps.idle()) break
        // Work started while the notice showed: take it away and wait again.
        hide()
      } else if (!asked) {
        const question = deps.ask(version)
        prompt = question
        void question.choice.then(choice => { if (prompt === question) answer = choice })
      }
    }
    if (!waiting) { waiting = true; await deps.report({ phase: 'waiting', version }) }
    // An answer ends the wait at once.
    await (prompt ? Promise.race([deps.sleep(IDLE_POLL_MS), prompt.choice.then(() => {})]) : deps.sleep(IDLE_POLL_MS))
  }
  await deps.report({ phase: 'restarting', version })
  try { await apply() } catch (error) { return fail(error) }
  return 'restarted'
}

/** Why this copy cannot install updates itself, for the machines page; null when it can. */
export function cannotInstallReason(): string | null {
  if (process.env.KYBERN_DISABLE_UPDATES === '1') return 'Updates are turned off for this copy of Kybern.'
  if (canSelfUpdate()) return null
  const runtime = path.basename(process.execPath).toLowerCase().replace(/\.exe$/, '')
  if (runtime === 'bun' || runtime === 'node') return 'Development build; rebuild it instead.'
  const install = linuxInstall()
  if (install?.kind === 'appimage') return `Kybern can’t write to ${path.dirname(install.image)}. Move the AppImage to a folder you own.`
  return 'This copy of Kybern can’t update itself. Update it the way it was installed.'
}

type UpdaterClient = Pick<KybernClient, 'call' | 'onNotification'>
let requestedInFlight: Promise<RequestedOutcome> | null = null
const listening = new WeakSet<object>()
const sleep = (ms: number) => new Promise<void>(resolve => setTimeout(resolve, ms))

/** Install what the daemon asked for, with this app's updater and state (the rail shows the progress). */
export function applyRequestedUpdate(client: UpdaterClient, automatic = false): Promise<RequestedOutcome> {
  if (requestedInFlight) return requestedInFlight
  if (installInFlight || state.phase === 'installing') return Promise.resolve('busy')
  requestedInFlight = runRequestedUpdate({
    check: async () => {
      const result = await readTrack(desktopVersion(), await currentTrack(), feed())
      return result?.entry ? { version: result.version, notes: result.notes, entry: result.entry } : null
    },
    prepare: async (update) => {
      pending = update
      set({ phase: 'installing', version: update.version, notes: update.notes, progress: null, error: null, errorKind: null, announcementOpen: false, unpublished: null, manual: null })
      try { return await prepareInstall(update, (progress) => set({ progress })) }
      catch (error) { set({ phase: 'available', progress: null, error: describe(error), errorKind: 'install' }); throw error }
    },
    idle: async () => { try { return activityIdle(await client.call('daemon.activity', {}), automatic) } catch { return false } },
    report: (params) => client.call('app_updater.report', params).then(() => {}, () => {}),
    notice: async (version) => {
      const { toast } = await import('../shell/toasts.tsx')
      toast.info(`Updating Kybern to ${version} — restarting…`, { id: RESTART_TOAST, duration: Infinity })
      return () => toast.dismiss(RESTART_TOAST)
    },
    sinceInput: () => sinceUserInput(),
    ask: (version) => askToRestart(version),
    sleep,
  }).finally(() => { requestedInFlight = null })
  return requestedInFlight
}

/** The kit toast as a prompt: Restart now or Later; its close button is Later too. */
function askToRestart(version: string): RestartPrompt {
  let settle: (choice: 'now' | 'later') => void = () => {}
  const choice = new Promise<'now' | 'later'>(resolve => { settle = resolve })
  let hidden = false
  void import('../shell/toasts.tsx').then(({ toast }) => {
    if (hidden) return
    toast.info(`Kybern ${version} is ready`, {
      id: RESTART_TOAST,
      description: 'Restart Kybern to finish updating.',
      duration: Infinity,
      cancel: { label: 'Later', onClick: () => settle('later') },
      action: { label: 'Restart now', onClick: () => settle('now') },
      onClose: () => settle('later'),
    })
  })
  return {
    choice,
    hide: () => {
      hidden = true
      void import('../shell/toasts.tsx').then(({ toast }) => toast.dismiss(RESTART_TOAST))
    },
  }
}

/**
 * Take update requests from this machine's daemon on `client`, the window's local connection. Call it on every
 * (re)connect: the daemon forgets a window when its connection closes. An older daemon has no such method.
 */
export function takeUpdateRequests(client: UpdaterClient) {
  if (!listening.has(client)) {
    listening.add(client)
    client.onNotification(APP_UPDATE_REQUESTED_NOTIFICATION, (params) => {
      void applyRequestedUpdate(client, (params as AppUpdateRequestedNotification | null)?.automatic === true)
    })
  }
  let version: string
  try { version = desktopVersion() } catch { return Promise.resolve() }
  const reason = cannotInstallReason()
  return client.call('app_updater.attach', { version, can_install: reason === null, ...(reason ? { reason } : {}) }).then(() => {}, () => {})
}

/** Hide this version's launch announcement across app restarts. */
export function dismissUpdateAnnouncement() {
  if (state.version) storeDismissedUpdateVersion(state.version)
  set({ announcementOpen: false })
}
export function openUpdateDetails() {
  if (!state.version) return
  storeDismissedUpdateVersion(state.version)
  set({ announcementOpen: false, detailsOpen: true })
}
export const closeUpdateDetails = () => set({ detailsOpen: false })

/** Load the app version and, in packaged builds, check shortly after launch and every few hours after that. */
export function startAppUpdateChecks() {
  loadAppVersion()
  // Installers of earlier updates cannot remove themselves.
  if (process.platform === 'win32') void removeStaleInstallers()
  if (!canCheckForUpdates() || timer) return
  const schedule = (delay: number) => {
    if (timer) clearTimeout(timer)
    timer = setTimeout(() => { void checkForAppUpdate().finally(() => schedule(CHECK_INTERVAL)) }, delay)
  }
  schedule(Number(process.env.KYBERN_UPDATE_FIRST_CHECK_MS) || FIRST_CHECK_DELAY)
}

function describe(error: unknown): string {
  const text = error instanceof Error ? error.message : String(error)
  // The feed is briefly absent while a release is being published, and unreachable offline.
  if (/release JSON|fetch|network|timed out|dns|connect|Unable to connect/i.test(text)) return 'The release feed is unavailable right now. Check your connection or try again in a few minutes.'
  return text
}
const dismissedUpdateVersion = () => readPref<string | null>(DISMISSED_UPDATE_KEY, null) ?? dismissedInSession
function storeDismissedUpdateVersion(version: string) {
  dismissedInSession = version
  try { writePref(DISMISSED_UPDATE_KEY, version) } catch { /* An unavailable store must not block update controls. */ }
}
