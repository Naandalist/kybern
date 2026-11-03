// How a Linux build of Kybern GPUIX was installed, and the AppImage's stable
// copies of its daemon and CLI. `scripts/package-linux.ts` writes the format
// into `resources/package` of each tree it packs (`appimage`, `deb`, `rpm`), so
// the updater never guesses from paths or package databases.
import { accessSync, chmodSync, constants, copyFileSync, mkdirSync, readFileSync, renameSync, statSync, writeFileSync } from 'node:fs'
import { homedir } from 'node:os'
import path from 'node:path'

export type LinuxInstall =
  /** A running AppImage: `image` is the file the updater replaces. */
  | { kind: 'appimage'; image: string }
  | { kind: 'deb' | 'rpm' }
  /** An AppImage unpacked with `--appimage-extract`: nothing to replace in place. */
  | { kind: 'unpacked' }

const isFile = (file: string) => { try { return statSync(file).isFile() } catch { return false } }

/** The release AppImage's file name; the updater refuses one whose signature names another file. */
export const appImageName = (version: string, arch: string = process.arch) => `kybern-gpuix_${version}_${arch === 'arm64' ? 'aarch64' : 'x86_64'}.AppImage`

let current: LinuxInstall | null | undefined
/**
 * Null outside a packaged Linux build (a source run, macOS). The answer for this process is read once: the UI asks on every
 * render, and a package file read inside an AppImage mount is a FUSE round trip.
 */
export function linuxInstall(env?: NodeJS.ProcessEnv, execPath?: string, platform?: NodeJS.Platform): LinuxInstall | null {
  if (env === undefined && execPath === undefined && platform === undefined) return current === undefined ? (current = detectLinuxInstall()) : current
  return detectLinuxInstall(env, execPath, platform)
}

/** Whether the updater can rename a new image over `image`: it writes the new file beside it. */
export function imageDirWritable(image: string): boolean {
  try { accessSync(path.dirname(image), constants.W_OK); return true } catch { return false }
}

function detectLinuxInstall(env: NodeJS.ProcessEnv = process.env, execPath = process.execPath, platform = process.platform): LinuxInstall | null {
  if (platform !== 'linux' || !env.KYBERN_GPUIX_RESOURCES) return null
  let format: string
  try { format = readFileSync(path.join(env.KYBERN_GPUIX_RESOURCES, 'package'), 'utf8').trim() } catch { return null }
  if (format === 'deb' || format === 'rpm') return { kind: format }
  if (format !== 'appimage') return null
  // The AppImage runtime sets both. A value inherited from another AppImage's
  // environment names a different mount than the one this process runs from.
  const { APPIMAGE: image, APPDIR: appDir } = env
  if (image && appDir && isFile(image) && execPath.startsWith(`${appDir.replace(/\/+$/, '')}/`)) return { kind: 'appimage', image }
  return { kind: 'unpacked' }
}

/**
 * Copy the image's `kybernd` and `kybern` to `$XDG_DATA_HOME/kybern-gpuix/bin`, or, when that folder cannot be written,
 * to `$XDG_RUNTIME_DIR/kybern-gpuix/bin` (a per-login tmpfs). An AppImage is mounted only while the app runs, but
 * kybernd outlives the app and a linked `kybern` command must survive the next launch's new mount path. Copies are
 * refreshed by rename when the version stamp changes, so a running daemon keeps its file. Returns null when neither
 * folder can be written; the caller then shows an error, because the daemon must never run from the mount.
 */
export function stageAppImageCommands(appDir: string, version: string, dataHome = process.env.XDG_DATA_HOME || path.join(homedir(), '.local/share'), runtimeDir = process.env.XDG_RUNTIME_DIR, image = process.env.APPIMAGE): { kybernd: string; kybern: string } | null {
  const folders = [path.join(dataHome, 'kybern-gpuix/bin'), ...(runtimeDir ? [path.join(runtimeDir, 'kybern-gpuix/bin')] : [])]
  for (const dir of folders) {
    const staged = stageInto(dir, appDir, version)
    if (staged) { if (image) nameImage(dir, image); return staged }
  }
  return null
}

/**
 * Write the image these copies came from to `appimage` beside them. The daemon outlives the app, and while the app is
 * closed it updates this image itself (`crates/kybern-daemon/src/app_update`); its own environment no longer names it.
 */
function nameImage(dir: string, image: string) {
  const file = path.join(dir, 'appimage')
  try {
    if (readFileSync(file, 'utf8').trim() === image) return
  } catch { /* First launch, or an older version staged these. */ }
  try {
    const temp = path.join(dir, `.appimage.${process.pid}.new`)
    writeFileSync(temp, `${image}\n`)
    renameSync(temp, file)
  } catch { /* The daemon then leaves the image for the app to update. */ }
}

function stageInto(dir: string, appDir: string, version: string): { kybernd: string; kybern: string } | null {
  const names = ['kybernd', 'kybern'] as const
  try {
    const stamp = `${version} ${names.map(name => statSync(path.join(appDir, name)).size).join(' ')}`
    const stampFile = path.join(dir, 'stamp')
    let current = ''
    try { current = readFileSync(stampFile, 'utf8').trim() } catch { /* First launch. */ }
    if (current !== stamp || !names.every(name => isFile(path.join(dir, name)))) {
      mkdirSync(dir, { recursive: true })
      for (const name of names) {
        const temp = path.join(dir, `.${name}.${process.pid}.new`)
        copyFileSync(path.join(appDir, name), temp)
        chmodSync(temp, 0o755)
        renameSync(temp, path.join(dir, name))
      }
      writeFileSync(stampFile, `${stamp}\n`)
    }
    return { kybernd: path.join(dir, 'kybernd'), kybern: path.join(dir, 'kybern') }
  } catch { return null }
}

/** The command a deb or rpm install updates with, run in the folder holding the downloaded `file`. */
export function packageCommand(kind: 'deb' | 'rpm', file: string, zypper = isFile('/usr/bin/zypper')): string {
  if (kind === 'deb') return `sudo apt install ./${file}`
  // The packages are not GPG-signed; dnf installs a local file without asking, zypper needs the flag.
  return zypper ? `sudo zypper install --allow-unsigned-rpm ./${file}` : `sudo dnf install ./${file}`
}
