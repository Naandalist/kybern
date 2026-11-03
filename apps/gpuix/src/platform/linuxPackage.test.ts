// Linux install detection and the AppImage's stable command copies, on scratch folders.
import { afterEach, expect, test } from 'bun:test'
import { mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { imageDirWritable, linuxInstall, packageCommand, stageAppImageCommands } from './linuxPackage.ts'

let root = ''
afterEach(() => { if (root) rmSync(root, { recursive: true, force: true }) })

function tree(format: string | null) {
  root = mkdtempSync(path.join(tmpdir(), 'kybern-linux-'))
  const app = path.join(root, 'mount/usr/lib/kybern-gpuix')
  mkdirSync(path.join(app, 'resources'), { recursive: true })
  if (format) writeFileSync(path.join(app, 'resources/package'), `${format}\n`)
  writeFileSync(path.join(root, 'Kybern.AppImage'), 'image')
  return { app, exe: path.join(app, 'kybern-gpuix'), resources: path.join(app, 'resources') }
}

test('reads the format each package records, and nothing for source runs or macOS', () => {
  const { exe, resources } = tree('deb')
  expect(linuxInstall({ KYBERN_GPUIX_RESOURCES: resources }, exe, 'linux')).toEqual({ kind: 'deb' })
  writeFileSync(path.join(resources, 'package'), 'rpm\n')
  expect(linuxInstall({ KYBERN_GPUIX_RESOURCES: resources }, exe, 'linux')).toEqual({ kind: 'rpm' })
  expect(linuxInstall({}, exe, 'linux')).toBeNull()
  expect(linuxInstall({ KYBERN_GPUIX_RESOURCES: resources }, exe, 'darwin')).toBeNull()
  writeFileSync(path.join(resources, 'package'), 'snap\n')
  expect(linuxInstall({ KYBERN_GPUIX_RESOURCES: resources }, exe, 'linux')).toBeNull()
})

test('an AppImage replaces its image only while running from that image’s mount', () => {
  const { exe, resources } = tree('appimage')
  const image = path.join(root, 'Kybern.AppImage')
  const mount = path.join(root, 'mount')
  expect(linuxInstall({ KYBERN_GPUIX_RESOURCES: resources, APPIMAGE: image, APPDIR: mount }, exe, 'linux')).toEqual({ kind: 'appimage', image })
  // Unpacked with --appimage-extract, or with APPIMAGE/APPDIR inherited from another AppImage.
  expect(linuxInstall({ KYBERN_GPUIX_RESOURCES: resources }, exe, 'linux')).toEqual({ kind: 'unpacked' })
  expect(linuxInstall({ KYBERN_GPUIX_RESOURCES: resources, APPIMAGE: image, APPDIR: path.join(root, 'other') }, exe, 'linux')).toEqual({ kind: 'unpacked' })
  expect(linuxInstall({ KYBERN_GPUIX_RESOURCES: resources, APPIMAGE: path.join(root, 'gone.AppImage'), APPDIR: mount }, exe, 'linux')).toEqual({ kind: 'unpacked' })
})

test('copies kybernd and kybern out of the image once per version', () => {
  const { app } = tree('appimage')
  writeFileSync(path.join(app, 'kybernd'), 'daemon 1')
  writeFileSync(path.join(app, 'kybern'), 'cli 1')
  const data = path.join(root, 'data')
  const staged = stageAppImageCommands(app, '0.16.0', data)!
  expect(staged).toEqual({ kybernd: path.join(data, 'kybern-gpuix/bin/kybernd'), kybern: path.join(data, 'kybern-gpuix/bin/kybern') })
  expect(readFileSync(staged.kybernd, 'utf8')).toBe('daemon 1')
  expect(statSync(staged.kybern).mode & 0o777).toBe(0o755)
  // Same version and sizes: the copies (and the running daemon's file) are left alone.
  const before = statSync(staged.kybernd).ino
  expect(stageAppImageCommands(app, '0.16.0', data)).toEqual(staged)
  expect(statSync(staged.kybernd).ino).toBe(before)
  // A new release replaces them by rename.
  writeFileSync(path.join(app, 'kybernd'), 'daemon 2')
  stageAppImageCommands(app, '0.16.1', data)
  expect(readFileSync(staged.kybernd, 'utf8')).toBe('daemon 2')
  expect(statSync(staged.kybernd).ino).not.toBe(before)
  // The copies name the image they came from, for the daemon to update while the app is closed.
  const image = path.join(root, 'Kybern.AppImage')
  stageAppImageCommands(app, '0.16.1', data, undefined, image)
  expect(readFileSync(path.join(data, 'kybern-gpuix/bin/appimage'), 'utf8').trim()).toBe(image)
  // Neither folder writable: null, so the caller shows an error instead of running the mounted copies.
  writeFileSync(path.join(root, 'file'), '')
  expect(stageAppImageCommands(app, '0.16.1', path.join(root, 'file'), '')).toBeNull()
  expect(stageAppImageCommands(app, '0.16.1', path.join(root, 'file'), path.join(root, 'file'))).toBeNull()
})

test('an unwritable data folder stages into the runtime folder instead', () => {
  const { app } = tree('appimage')
  writeFileSync(path.join(app, 'kybernd'), 'daemon')
  writeFileSync(path.join(app, 'kybern'), 'cli')
  writeFileSync(path.join(root, 'file'), '')
  const run = path.join(root, 'run')
  const staged = stageAppImageCommands(app, '0.16.0', path.join(root, 'file'), run)!
  expect(staged).toEqual({ kybernd: path.join(run, 'kybern-gpuix/bin/kybernd'), kybern: path.join(run, 'kybern-gpuix/bin/kybern') })
  expect(readFileSync(staged.kybern, 'utf8')).toBe('cli')
  // Neither copy is ever the mounted file.
  expect(staged.kybernd.startsWith(app)).toBe(false)
})

test('a folder the user cannot write blocks in-place updates', () => {
  tree('appimage')
  expect(imageDirWritable(path.join(root, 'Kybern.AppImage'))).toBe(true)
  expect(imageDirWritable(path.join(root, 'missing/Kybern.AppImage'))).toBe(false)
})

test('names the install command of each package manager', () => {
  expect(packageCommand('deb', 'kybern-gpuix_0.16.0_x86_64.deb')).toBe('sudo apt install ./kybern-gpuix_0.16.0_x86_64.deb')
  expect(packageCommand('rpm', 'kybern-gpuix_0.16.0_x86_64.rpm', false)).toBe('sudo dnf install ./kybern-gpuix_0.16.0_x86_64.rpm')
  expect(packageCommand('rpm', 'kybern-gpuix_0.16.0_x86_64.rpm', true)).toBe('sudo zypper install --allow-unsigned-rpm ./kybern-gpuix_0.16.0_x86_64.rpm')
})
