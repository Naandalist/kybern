// Images reach the native <img> as files sized for their slot. A `data:` URL was
// copied into JS strings, the native image cache key and every prop update, and
// every source decoded at full size: a 2880×1800 screenshot is 20 MB of pixels
// for a 280 pt reply image, and a reply that streams several of them re-decoded
// them as the image cache churned (`perf/memlog/README.md`). Each source is
// written once per content (`data:` URLs and fetched bytes) or used in place
// (local files), plus a copy at most twice its slot wide (`sips`, ImageIO); the
// viewer and image tabs read the original. The folder is pruned to CACHE_BYTES at
// launch, oldest first.
//
// The folder can disappear while the app runs (a disk cleanup, ADE-122): a write
// that finds it gone makes it again and tries once more, and bytes that still
// cannot be written there go to this process's own folder in the OS temp dir
// (made with mkdtemp, removed on exit), so a fetched image still shows.
import { spawn } from 'node:child_process'
import { existsSync, mkdirSync, mkdtempSync, readdirSync, rmSync, statSync, unlinkSync, utimesSync } from 'node:fs'
import { mkdir, rename, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { useEffect, useMemo, useState } from 'react'
import { imageSource } from '../../../../packages/kybern-client/src/responseImages.ts'
import { gpuixCacheDir } from '../platform/os.ts'

const CACHE_BYTES = 256 * 1024 * 1024
const MEMORY_ENTRIES = 512
/** `sips` processes at a time. */
const CONVERSIONS = 2
const ENABLED = process.env.KYBERN_IMAGE_FILES !== '0'
const EXTENSIONS: Record<string, string> = { 'image/png': 'png', 'image/jpeg': 'jpg', 'image/gif': 'gif', 'image/webp': 'webp', 'image/avif': 'avif' }

let folder: string | null = null
function cacheFolder() {
  if (folder) return folder
  folder = gpuixCacheDir('image-cache', 'images')
  mkdirSync(folder, { recursive: true })
  prune(folder)
  return folder
}

/** The cache folder, made again when something deleted it since. */
function ensureCacheFolder(): string {
  const dir = cacheFolder()
  if (!existsSync(dir)) mkdirSync(dir, { recursive: true })
  return dir
}

const missing = (error: unknown) => (error as NodeJS.ErrnoException | null)?.code === 'ENOENT'

/** Run a write into the cache folder; when the folder is gone, make it again and run it once more. */
async function intoCache<T>(write: () => Promise<T>): Promise<T> {
  try {
    return await write()
  } catch (error) {
    if (!missing(error)) throw error
    await mkdir(cacheFolder(), { recursive: true })
    return await write()
  }
}

/** This process's own folders in the OS temp dir, removed when it exits. */
const tempFolders: string[] = []
/** A folder in the OS temp dir for when the cache folder cannot be written. `mkdtemp` makes it, so its name cannot be
 *  guessed and no other user's file or link can be waiting there in the shared temp dir; one deleted since is made anew
 *  the same way, never again at a known path. Removed on exit. */
export function tempImageFolder(): string {
  const current = tempFolders.at(-1)
  if (current && existsSync(current)) return current
  const folder = mkdtempSync(path.join(tmpdir(), 'kybern-gpuix-images-'))
  if (!tempFolders.length) process.once('exit', () => { for (const dir of tempFolders) try { rmSync(dir, { recursive: true, force: true }) } catch { /* Left for the OS to clear. */ } })
  tempFolders.push(folder)
  return folder
}

/** Where fetched bytes go when the cache folder cannot be written at all: still a file (never a `data:` URL). */
async function fallbackFile(name: string, bytes: Uint8Array): Promise<string> {
  const file = path.join(tempImageFolder(), name)
  if (!existsSync(file)) {
    const temp = `${file}.${++writes}.tmp`
    await writeFile(temp, bytes)
    await rename(temp, file)
  }
  return file
}

/** Oldest first until the folder fits CACHE_BYTES; a used file is touched, so it counts as new. */
function prune(dir: string) {
  try {
    const files = readdirSync(dir).map(name => { const file = path.join(dir, name); const stat = statSync(file); return { file, size: stat.size, at: stat.mtimeMs } })
    let total = files.reduce((sum, f) => sum + f.size, 0)
    for (const f of files.sort((a, b) => a.at - b.at)) {
      if (total <= CACHE_BYTES) break
      try { unlinkSync(f.file); total -= f.size } catch { /* In use or gone. */ }
    }
  } catch { /* An unreadable cache folder only costs disk space. */ }
}

/** A file in the image cache for the native layer to write (a live visual's frame); pruned with the rest. The folder
 *  exists when this returns, even if something deleted it since launch. */
export function imageCachePath(name: string): string {
  try { return path.join(ensureCacheFolder(), name) } catch { return path.join(tempImageFolder(), name) }
}

const hex = (value: string | Uint8Array) => Bun.hash(value).toString(16).padStart(16, '0')
const touch = (file: string) => { try { const now = new Date(); utimesSync(file, now, now) } catch { /* Pruned later. */ } }

let writes = 0
/** Fetched image bytes as a cached file; returns its path. */
export async function storeImageBytes(bytes: Uint8Array, media: string): Promise<string> {
  const name = `${hex(bytes)}.${EXTENSIONS[media] ?? 'img'}`
  let file: string
  try { file = path.join(cacheFolder(), name) } catch { return fallbackFile(name, bytes) }
  if (existsSync(file)) { touch(file); return file }
  try {
    // Renamed into place, so a reader never sees a partial file. A temp file the folder lost with it is written again.
    await intoCache(async () => {
      const temp = `${file}.${process.pid}.${++writes}.tmp`
      await writeFile(temp, bytes)
      await rename(temp, file)
    })
    return file
  } catch {
    // An unwritable cache folder still shows the image.
    return fallbackFile(name, bytes)
  }
}

let running = 0
const waiting: (() => void)[] = []
async function convert(input: string, output: string, width: number, jpeg: boolean) {
  // `sips` is macOS's: elsewhere the original file is the inline image too (callers fall back to it on `false`).
  if (process.platform !== 'darwin') return false
  if (running >= CONVERSIONS) await new Promise<void>(resolve => waiting.push(resolve))
  running++
  try {
    const once = async () => {
      // `sips` cannot make the folder it writes into: one deleted since launch is made again first.
      ensureCacheFolder()
      const temp = `${output}.${process.pid}.${++writes}.tmp`
      const ok = await new Promise<boolean>(resolve => {
        const child = spawn('/usr/bin/sips', ['--resampleWidth', String(width), '-s', 'format', jpeg ? 'jpeg' : 'png', input, '--out', temp], { stdio: 'ignore' })
        child.on('error', () => resolve(false))
        child.on('exit', code => resolve(code === 0))
      })
      if (ok) await rename(temp, output)
      else try { unlinkSync(temp) } catch { /* Never written. */ }
      return ok
    }
    // The narrow copy is optional: when it cannot be made, callers show the original.
    return await intoCache(once).catch(() => false)
  } finally {
    running--
    waiting.shift()?.()
  }
}

export type ImageFiles = { inline: string; full: string }
const imageKey = (source: string, maxPx: number) => `${maxPx}:${source.length > 4096 ? hex(source) : source}`
const memory = new Map<string, Promise<ImageFiles | null>>()
/** Settled `memory` entries, so a row remounted by the virtual list paints its image on its first frame. */
const settled = new Map<string, ImageFiles | null>()

/** The original and a copy at most `maxPx` wide, as files. `null` when the source is not a readable image file or
 *  `data:` URL (callers keep the source as it was). `width`: the source's pixel width, when known. */
export function imageFiles(source: string, maxPx: number, width: number | null, key = imageKey(source, maxPx)): Promise<ImageFiles | null> {
  let pending = memory.get(key)
  if (pending) { memory.delete(key); memory.set(key, pending); return pending }
  pending = prepare(source, maxPx, width).catch(() => null)
  pending.then(files => { if (memory.has(key)) settled.set(key, files) })
  memory.set(key, pending)
  while (memory.size > MEMORY_ENTRIES) { const oldest = memory.keys().next().value!; memory.delete(oldest); settled.delete(oldest) }
  return pending
}

async function prepare(source: string, maxPx: number, width: number | null): Promise<ImageFiles | null> {
  if (!ENABLED) return null
  const target = imageSource(source)
  if (!target || target.kind === 'remote') return null
  let full: string, id: string, media: string
  if (target.kind === 'inline') {
    const comma = target.value.indexOf(',')
    media = target.value.slice(5, target.value.indexOf(';'))
    full = await storeImageBytes(Buffer.from(target.value.slice(comma + 1), 'base64'), media)
    id = path.basename(full).replace(/\.\w+$/, '')
  } else {
    if (!path.isAbsolute(target.value)) return null
    const stat = statSync(target.value)
    full = target.value
    id = hex(`${full}:${stat.size}:${stat.mtimeMs}`)
    media = /\.jpe?g$/i.test(full) ? 'image/jpeg' : /\.gif$/i.test(full) ? 'image/gif' : 'image/png'
  }
  // Small enough already, or animated (a GIF keeps its frames).
  if (process.platform !== 'darwin' || media === 'image/gif' || (width != null && width <= maxPx)) return { inline: full, full }
  const jpeg = media === 'image/jpeg'
  const inline = path.join(cacheFolder(), `${id}-w${maxPx}.${jpeg ? 'jpg' : 'png'}`)
  if (existsSync(inline)) { touch(inline); return { inline, full } }
  return await convert(full, inline, maxPx, jpeg) ? { inline, full } : { inline: full, full }
}

/** `imageFiles` for a component: `null` while the files are prepared, then the files, or the source as it was. */
export function useImageFiles(source: string | null, maxPx: number, width: number | null): ImageFiles | null {
  const key = useMemo(() => source ? imageKey(source, maxPx) : '', [source, maxPx])
  const known = (files: ImageFiles | null | undefined) => files === undefined ? null : files ?? (source ? { inline: source, full: source } : null)
  const [state, setState] = useState<{ key: string; files: ImageFiles | null }>(() => ({ key, files: known(settled.get(key)) }))
  useEffect(() => {
    if (!source) return
    if (settled.has(key)) { setState(s => s.key === key ? s : { key, files: known(settled.get(key)) }); return }
    let alive = true
    void imageFiles(source, maxPx, width, key).then(files => { if (alive) setState({ key, files: files ?? { inline: source, full: source } }) })
    return () => { alive = false }
    // `key` is `source` (hashed when long) and `maxPx`.
  }, [key])
  return state.key === key ? state.files : known(settled.get(key))
}
