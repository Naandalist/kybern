// The updater's pure parts: signature checks (Tauri's minisign format), feed
// parsing (GPUIX's own platform key), and the bundle swap, on scratch folders.
import { afterEach, expect, test } from 'bun:test'
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, utimesSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { appImageName } from './linuxPackage.ts'
import { verifyMinisign } from './minisign.ts'
import { relaunchEnv } from '../shell/daemon.ts'
import { assertBundleVersion, assertSignedRelease, assertWindowsInstaller, manualUpdate, PLATFORM_KEY, platformKey, readFeed, readTrack, removeStaleInstallers, replaceAppImage, replaceBundle, stageWindowsInstaller, windowsInstall, windowsInstallerCommand } from './update.ts'
import { archive, fakeBundle, testKey } from './verify.feed.ts'

let root = ''
afterEach(() => { if (root) rmSync(root, { recursive: true, force: true }) })

test('accepts a tauri-signed file and rejects tampering or another key', () => {
  const key = testKey(), other = testKey()
  const data = Buffer.from('kybern update payload')
  const signature = key.signFile(data, 'update.app.tar.gz')
  expect(verifyMinisign(data, signature, key.pubkey).trustedComment).toContain('file:update.app.tar.gz')
  expect(() => verifyMinisign(Buffer.from('kybern update payloaD'), signature, key.pubkey)).toThrow('does not match the download')
  expect(() => verifyMinisign(data, signature, other.pubkey)).toThrow('signed with a different key')
  // An edited trusted comment breaks the global signature.
  const lines = Buffer.from(signature, 'base64').toString().split('\n')
  lines[2] = 'trusted comment: timestamp:0\tfile:evil'
  expect(() => verifyMinisign(data, Buffer.from(lines.join('\n')).toString('base64'), key.pubkey)).toThrow('trusted comment was altered')
})

test('reads only the GPUIX platform entry and ignores older versions', async () => {
  const feed = (body: unknown) => (async () => Response.json(body)) as unknown as typeof fetch
  const platforms = { 'darwin-aarch64': { url: 'https://example/desktop.tar.gz', signature: 's' } }
  expect(await readFeed('0.9.1', ['x'], feed({ version: '0.9.1', platforms }))).toBeNull()
  expect(await readFeed('0.9.1', ['x'], feed({ version: '0.9.2', platforms }))).toMatchObject({ version: '0.9.2', entry: null })
  const ours = await readFeed('0.9.1', ['x'], feed({ version: 'v0.9.2', notes: 'n', platforms: { ...platforms, [PLATFORM_KEY]: { url: 'https://example/gpuix.tar.gz', signature: 'g' } } }))
  expect(ours).toEqual({ version: '0.9.2', notes: 'n', entry: { url: 'https://example/gpuix.tar.gz', signature: 'g' } })
})

test('each update track reads its own feeds and never offers a downgrade', async () => {
  const entry = { url: 'https://example/gpuix.tar.gz', signature: 'g' }
  // `stable` and `dev` stand in for latest.json and latest-dev.json; a missing feed answers 404.
  const feeds = (stable: string | null, dev: string | null) => (async (url: string) => {
    const version = url === 'stable' ? stable : dev
    return version ? Response.json({ version, notes: url, platforms: { [PLATFORM_KEY]: entry } }) : new Response('', { status: 404 })
  }) as unknown as typeof fetch
  const both = { stable: ['stable'], dev: ['dev'] }
  // Stable never sees a dev build.
  expect(await readTrack('0.20.0', 'stable', both, feeds('0.20.0', '0.21.0-dev.2'))).toBeNull()
  expect(await readTrack('0.20.0', 'stable', both, feeds('0.20.1', '0.21.0-dev.2'))).toMatchObject({ version: '0.20.1', notes: 'stable' })
  // Dev takes the newer of the two feeds: the dev build, or a stable release that overtook it.
  expect(await readTrack('0.20.0', 'dev', both, feeds('0.20.1', '0.21.0-dev.2'))).toMatchObject({ version: '0.21.0-dev.2', notes: 'dev', entry })
  expect(await readTrack('0.21.0-dev.2', 'dev', both, feeds('0.20.1', '0.21.0-dev.10'))).toMatchObject({ version: '0.21.0-dev.10' })
  expect(await readTrack('0.21.0-dev.2', 'dev', both, feeds('0.21.0', '0.21.0-dev.2'))).toMatchObject({ version: '0.21.0', notes: 'stable' })
  // Dev → Stable keeps the dev build: an older stable is no update, a newer one is.
  expect(await readTrack('0.21.0-dev.2', 'stable', both, feeds('0.20.1', '0.21.0-dev.3'))).toBeNull()
  expect(await readTrack('0.21.0-dev.2', 'stable', both, feeds('0.21.0', '0.21.0-dev.3'))).toMatchObject({ version: '0.21.0' })
  // The dev feed is missing until the first dev release; Dev still reads the stable one. Only both failing is an error.
  expect(await readTrack('0.20.0', 'dev', both, feeds('0.20.1', null))).toMatchObject({ version: '0.20.1' })
  expect(await readTrack('0.20.1', 'dev', both, feeds('0.20.1', null))).toBeNull()
  await expect(readTrack('0.20.0', 'dev', both, feeds(null, null))).rejects.toThrow('404')
  await expect(readTrack('0.20.0', 'stable', both, feeds(null, '0.21.0-dev.2'))).rejects.toThrow('404')
})

// The swap reads the new bundle's identifier with macOS's PlistBuddy, so this runs on macOS only.
test.skipIf(process.platform !== 'darwin')('swaps the bundle in place and refuses another app’s archive', async () => {
  root = mkdtempSync(path.join(tmpdir(), 'kybern-update-'))
  const installed = fakeBundle(path.join(root, 'Applications'), '0.9.1')
  const next = fakeBundle(path.join(root, 'next'), '0.9.2')
  const expected = { version: '0.9.2', current: '0.9.1', track: 'stable' as const }
  // An older signed build, or one that is not the release the feed named, never replaces the app.
  const older = archive(fakeBundle(path.join(root, 'older'), '0.9.0'), path.join(root, 'older.tar.gz'))
  await expect(replaceBundle(installed, older, { ...expected, version: '0.9.0' })).rejects.toThrow('not newer')
  await expect(replaceBundle(installed, older, expected)).rejects.toThrow('contains Kybern 0.9.0, not 0.9.2')
  expect(readFileSync(path.join(installed, 'Contents/Resources/version'), 'utf8')).toBe('0.9.1')
  expect(readdirSync(path.join(root, 'Applications'))).toEqual(['Kybern GPUIX.app'])
  await replaceBundle(installed, archive(next, path.join(root, 'next.tar.gz')), expected)
  expect(readFileSync(path.join(installed, 'Contents/Resources/version'), 'utf8')).toBe('0.9.2')
  // Nothing staged is left beside the app.
  expect(readdirSync(path.join(root, 'Applications'))).toEqual(['Kybern GPUIX.app'])
  const foreign = fakeBundle(path.join(root, 'foreign'), '9.9.9')
  writeFileSync(path.join(foreign, 'Contents/Info.plist'), readFileSync(path.join(foreign, 'Contents/Info.plist'), 'utf8').replace('dev.kybern.gpuix', 'dev.kybern.desktop'))
  await expect(replaceBundle(installed, archive(foreign, path.join(root, 'foreign.tar.gz')), { ...expected, version: '9.9.9', current: '0.9.2' })).rejects.toThrow('The update is for dev.kybern.desktop, not this one')
  expect(readFileSync(path.join(installed, 'Contents/Resources/version'), 'utf8')).toBe('0.9.2')
})

test('a macOS bundle must be the release, newer than this one and on this track', () => {
  const stable = { version: '0.21.4', current: '0.21.3', track: 'stable' as const }
  expect(() => assertBundleVersion('0.21.4\n', stable)).not.toThrow()
  expect(() => assertBundleVersion('0.21.5', stable)).toThrow('not 0.21.4')
  expect(() => assertBundleVersion('', stable)).toThrow('unknown version')
  expect(() => assertBundleVersion('0.21.4', { ...stable, current: '0.21.4' })).toThrow('not newer')
  expect(() => assertBundleVersion('0.21.2', { ...stable, version: '0.21.2' })).toThrow('not newer')
  expect(() => assertBundleVersion('0.21.5-dev.1', { ...stable, version: '0.21.5-dev.1' })).toThrow('Stable')
  expect(() => assertBundleVersion('0.21.5-dev.1', { ...stable, version: '0.21.5-dev.1', track: 'dev' })).not.toThrow()
})

test('each platform reads its own feed entry', () => {
  expect(platformKey('darwin', 'arm64')).toBe('darwin-aarch64-gpuix')
  expect(platformKey('darwin', 'x64')).toBe('darwin-x86_64-gpuix')
  expect(platformKey('win32', 'x64')).toBe('windows-x86_64-gpuix')
  expect(platformKey('linux', 'x64')).toBe('linux-x86_64-gpuix')
})

test('only an installed Windows copy updates itself', () => {
  const dir = 'C:\\Users\\me\\AppData\\Local\\Programs\\Kybern GPUIX'
  const installed = (file: string) => file === `${dir}\\uninstall.exe`
  expect(windowsInstall(`${dir}\\Kybern GPUIX.exe`, installed)).toBe(dir)
  // A loose copy has no uninstaller beside it; bun and other programs are never the app.
  expect(windowsInstall('C:\\Downloads\\Kybern GPUIX.exe', installed)).toBeNull()
  expect(windowsInstall(`${dir}\\bun.exe`, installed)).toBeNull()
})

test('a downloaded Windows installer is staged only if it is a PE file', async () => {
  root = mkdtempSync(path.join(tmpdir(), 'kybern-update-'))
  const file = await stageWindowsInstaller(Buffer.from('MZ\x90\x00installer'), root)
  expect(path.dirname(path.dirname(file))).toBe(root)
  expect(readFileSync(file).subarray(0, 2).toString()).toBe('MZ')
  await expect(stageWindowsInstaller(Buffer.from('#!/bin/sh\nrm -rf ~'), root)).rejects.toThrow('not a Windows installer')
})

test('old staged installers are removed, recent ones and other folders are kept', async () => {
  root = mkdtempSync(path.join(tmpdir(), 'kybern-update-'))
  const old = path.join(root, 'kybern-gpuix-update-old'), fresh = path.join(root, 'kybern-gpuix-update-new'), other = path.join(root, 'other')
  for (const dir of [old, fresh, other]) mkdirSync(dir)
  const longAgo = new Date(Date.now() - 2 * 60 * 60 * 1000)
  utimesSync(old, longAgo, longAgo)
  utimesSync(other, longAgo, longAgo)
  await removeStaleInstallers(root)
  expect([old, fresh, other].map(existsSync)).toEqual([false, true, true])
})

test('a signed Windows installer must be this release’s GPUIX setup and newer than the running app', () => {
  const key = testKey(), data = Buffer.from('MZ installer')
  const comment = (file: string) => verifyMinisign(data, key.signFile(data, file), key.pubkey).trustedComment
  expect(() => assertWindowsInstaller(comment('kybern-gpuix_0.16.0_x64-setup.exe'), '0.16.0', '0.15.1')).not.toThrow()
  // Tauri's setup, an older GPUIX installer and the macOS archive all carry valid signatures from the same key.
  for (const file of ['Kybern_0.16.0_x64-setup.exe', 'kybern-gpuix_0.15.0_x64-setup.exe', 'kybern-gpuix_0.16.0_aarch64.app.tar.gz'])
    expect(() => assertWindowsInstaller(comment(file), '0.16.0', '0.15.1')).toThrow('Download it from the release page.')
  expect(() => assertWindowsInstaller('timestamp:1', '0.16.0', '0.15.1')).toThrow('not the build for this release')
  // The feed cannot talk the app into installing the version it already runs, or an older one.
  expect(() => assertWindowsInstaller(comment('kybern-gpuix_0.15.1_x64-setup.exe'), '0.15.1', '0.15.1')).toThrow('not newer')
  expect(() => assertWindowsInstaller(comment('kybern-gpuix_0.15.0_x64-setup.exe'), '0.15.0', '0.15.1')).toThrow('not newer')
})

test('the installer starts through Start-Process with an unquoted /D= folder last', () => {
  const { command, args } = windowsInstallerCommand("C:\\Users\\O'Brien\\Temp\\kybern-gpuix-update-x\\setup.exe", 'D:\\My Apps\\Kybern GPUIX\\', 'C:\\Windows')
  expect(command).toBe('C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe')
  expect(args.at(-1)).toBe("Start-Process -FilePath 'C:\\Users\\O''Brien\\Temp\\kybern-gpuix-update-x\\setup.exe' -ArgumentList '/S /UPDATE /D=D:\\My Apps\\Kybern GPUIX'")
  expect(windowsInstallerCommand('C:\\s.exe', 'E:\\', 'C:\\Windows').args.at(-1)).toContain("/D=E:\\'")
})

/** The first bytes of a type 2 AppImage: a 64-bit little-endian ELF header for `machine` (62 = x86-64) with `AI\x02` at offset 8. */
const appImage = (body: string, machine = 62) => {
  const header = Buffer.alloc(64)
  header.set([0x7f, 0x45, 0x4c, 0x46, 2, 1, 1, 0, 0x41, 0x49, 0x02])
  header.writeUInt16LE(machine, 18)
  return Buffer.concat([header, Buffer.from(body)])
}

test('replaces a running AppImage in place and refuses other files', async () => {
  root = mkdtempSync(path.join(tmpdir(), 'kybern-update-'))
  const image = path.join(root, 'Kybern-GPUIX-x86_64.AppImage')
  writeFileSync(image, appImage('0.9.1'))
  chmodSync(image, 0o700)
  const before = statSync(image).ino
  await replaceAppImage(image, appImage('0.9.2'), 'kybern-gpuix_0.9.2_x86_64.AppImage', '0.9.2', 'x64')
  expect(readFileSync(image).subarray(64).toString()).toBe('0.9.2')
  // A new file renamed over the old one: the mounted image keeps running from its own inode, with the old file's mode.
  expect(statSync(image).ino).not.toBe(before)
  expect(statSync(image).mode & 0o777).toBe(0o700)
  expect(readdirSync(root)).toEqual(['Kybern-GPUIX-x86_64.AppImage'])
  // The Tauri AppImage carries the same signing key; the signed file name tells them apart. So does the exact version:
  // an older GPUIX release's AppImage matches the pattern but not this release.
  for (const name of ['kybern-9.9.9-x86_64-unknown-linux-gnu.AppImage', 'kybern-gpuix_0.9.1_x86_64.AppImage', 'kybern-gpuix_0.9.2_aarch64.AppImage', 'kybern-gpuix_0.9.2_x86_64.AppImage.exe', 'x/kybern-gpuix_0.9.2_x86_64.AppImage', ''])
    await expect(replaceAppImage(image, appImage('9.9.9'), name, '0.9.2', 'x64')).rejects.toThrow('not a Kybern GPUIX 0.9.2 AppImage')
  await expect(replaceAppImage(image, Buffer.from('#!/bin/sh\necho hi\n'), 'kybern-gpuix_9.9.9_x86_64.AppImage', '9.9.9', 'x64')).rejects.toThrow('The update is not an AppImage')
  expect(readFileSync(image).subarray(64).toString()).toBe('0.9.2')
  expect(readdirSync(root)).toEqual(['Kybern-GPUIX-x86_64.AppImage'])
})

test('a downloaded AppImage must be an ELF for this machine before it is swapped in', async () => {
  root = mkdtempSync(path.join(tmpdir(), 'kybern-update-'))
  const image = path.join(root, 'Kybern.AppImage')
  writeFileSync(image, appImage('old'))
  const name = 'kybern-gpuix_0.9.2_x86_64.AppImage'
  // An AArch64 image, a 32-bit class, a big-endian encoding and a truncated header all carry the AppImage magic.
  const arm = appImage('arm', 183)
  const class32 = appImage('32'); class32[4] = 1
  const big = appImage('big'); big[5] = 2
  for (const bad of [arm, class32, big, appImage('x').subarray(0, 19)])
    await expect(replaceAppImage(image, bad, name, '0.9.2', 'x64')).rejects.toThrow('not an AppImage for x86-64')
  await expect(replaceAppImage(image, appImage('x64'), 'kybern-gpuix_0.9.2_aarch64.AppImage', '0.9.2', 'arm64')).rejects.toThrow('not an AppImage for arm64')
  await replaceAppImage(image, arm, 'kybern-gpuix_0.9.2_aarch64.AppImage', '0.9.2', 'arm64')
  expect(readFileSync(image).subarray(64).toString()).toBe('arm')
  expect(readdirSync(root)).toEqual(['Kybern.AppImage'])
})

test('a leftover staging file is neither overwritten nor deleted by a failed swap', async () => {
  root = mkdtempSync(path.join(tmpdir(), 'kybern-update-'))
  const image = path.join(root, 'Kybern.AppImage')
  writeFileSync(image, appImage('old'))
  const leftover = path.join(root, `.Kybern.AppImage.update-${process.pid}`)
  writeFileSync(leftover, 'someone else’s')
  await expect(replaceAppImage(image, appImage('new'), 'kybern-gpuix_0.9.2_x86_64.AppImage', '0.9.2', 'x64')).rejects.toThrow()
  expect(readFileSync(leftover, 'utf8')).toBe('someone else’s')
  expect(readFileSync(image).subarray(64).toString()).toBe('old')
})

test('a copy started by the app resolves its own daemon and CLI', () => {
  const env = { KYBERND: '/home/me/.local/share/kybern-gpuix/bin/kybernd', KYBERN_BUNDLED_CLI: '/x/kybern', KYBERN_GPUIX_PACKAGED_DAEMON: '/x/kybernd', APPIMAGE: '/a.AppImage', APPDIR: '/tmp/.mount_x', ARGV0: 'a', OWD: '/', HOME: '/home/me', KYBERN_DATA_DIR: '/d' }
  expect(relaunchEnv(env)).toEqual({ APPIMAGE: '/a.AppImage', APPDIR: '/tmp/.mount_x', ARGV0: 'a', OWD: '/', HOME: '/home/me', KYBERN_DATA_DIR: '/d' })
  // A relaunched image mounts afresh, so its own mount variables go too; the data folder and home stay.
  expect(relaunchEnv(env, { appImage: true })).toEqual({ HOME: '/home/me', KYBERN_DATA_DIR: '/d' })
  expect(env.KYBERND).toBeDefined()
})

test('deb and rpm installs get the package beside the feed’s AppImage and its install command', () => {
  const entry = { url: 'https://github.com/o/r/releases/download/v0.9.2/kybern-gpuix_0.9.2_x86_64.AppImage', signature: 's' }
  expect(manualUpdate(entry, { kind: 'deb' })).toEqual({ url: 'https://github.com/o/r/releases/download/v0.9.2/kybern-gpuix_0.9.2_x86_64.deb', file: 'kybern-gpuix_0.9.2_x86_64.deb', command: 'sudo apt install ./kybern-gpuix_0.9.2_x86_64.deb' })
  expect(manualUpdate(entry, { kind: 'rpm' })?.url).toBe('https://github.com/o/r/releases/download/v0.9.2/kybern-gpuix_0.9.2_x86_64.rpm')
  expect(manualUpdate(entry, { kind: 'rpm' })?.command).toMatch(/^sudo (dnf|zypper) install .*\.\/kybern-gpuix_0\.9\.2_x86_64\.rpm$/)
  // An unpacked AppImage can only download the new one; a running AppImage and macOS install it themselves.
  expect(manualUpdate(entry, { kind: 'unpacked' })).toEqual({ url: entry.url, file: 'kybern-gpuix_0.9.2_x86_64.AppImage', command: null })
  expect(manualUpdate(entry, { kind: 'appimage', image: '/x.AppImage' }, () => true)).toBeNull()
  // A running AppImage in a folder its user cannot write can only download the new one.
  expect(manualUpdate(entry, { kind: 'appimage', image: '/usr/share/x.AppImage' }, () => false)).toEqual({ url: entry.url, file: 'kybern-gpuix_0.9.2_x86_64.AppImage', command: null })
  expect(manualUpdate(entry, null)).toBeNull()
})

test('a signed AppImage must be this release’s GPUIX AppImage and newer than the running app', () => {
  const key = testKey(), data = Buffer.from('AppImage')
  const comment = (file: string) => verifyMinisign(data, key.signFile(data, file), key.pubkey).trustedComment
  expect(appImageName('0.16.0', 'x64')).toBe('kybern-gpuix_0.16.0_x86_64.AppImage')
  expect(() => assertSignedRelease(comment('kybern-gpuix_0.16.0_x86_64.AppImage'), appImageName('0.16.0', 'x64'), '0.16.0', '0.15.1')).not.toThrow()
  // Tauri's AppImage and an older GPUIX AppImage are signed with the same key.
  for (const file of ['kybern_0.16.0_amd64.AppImage', 'kybern-gpuix_0.15.0_x86_64.AppImage', 'kybern-gpuix_0.16.0_x64-setup.exe'])
    expect(() => assertSignedRelease(comment(file), appImageName('0.16.0', 'x64'), '0.16.0', '0.15.1')).toThrow('Download it from the release page.')
  expect(() => assertSignedRelease(comment('kybern-gpuix_0.15.1_x86_64.AppImage'), appImageName('0.15.1', 'x64'), '0.15.1', '0.15.1')).toThrow('not newer')
})
