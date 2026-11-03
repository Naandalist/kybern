// Its own process folder: `imageFiles.ts` reads KYBERN_CLIENT_CONFIG_DIR once.
import { expect, test } from 'bun:test'
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { gradientPng } from '../../visual/fixture.ts'

const dir = mkdtempSync(path.join(tmpdir(), 'kybern-image-files-'))
process.env.KYBERN_CLIENT_CONFIG_DIR = dir
const { imageCachePath, imageFiles, storeImageBytes, tempImageFolder } = await import('./imageFiles.ts')
/** A PNG's pixel width, from its header. */
const width = (file: string) => readFileSync(file).readUInt32BE(16)

test('a data URL becomes the original file and a copy at most maxPx wide', async () => {
  const png = gradientPng(1600, 900)
  const files = (await imageFiles(`data:image/png;base64,${Buffer.from(png).toString('base64')}`, 560, 1600))!
  expect(files.full.startsWith(path.join(dir, 'image-cache'))).toBe(true)
  expect(width(files.full)).toBe(1600)
  // The narrow copy is made with `sips` (macOS); elsewhere the original is shown inline.
  const mac = process.platform === 'darwin'
  expect(width(files.inline)).toBe(mac ? 560 : 1600)
  // The same bytes again: one original, one copy, no new work.
  expect(await imageFiles(`data:image/png;base64,${Buffer.from(png).toString('base64')}`, 560, 1600)).toEqual(files)
  expect(readdirSync(path.join(dir, 'image-cache')).filter(f => !f.endsWith('.tmp'))).toHaveLength(mac ? 2 : 1)
})

test('a local file narrow enough for its slot is used in place', async () => {
  const file = path.join(dir, 'small.png')
  writeFileSync(file, gradientPng(300, 200))
  expect(await imageFiles(file, 560, 300)).toEqual({ inline: file, full: file })
})

test('a relative path or a remote URL keeps its source', async () => {
  expect(await imageFiles('shots/a.png', 560, null)).toBeNull()
  expect(await imageFiles('https://example.com/a.png', 560, null)).toBeNull()
})

test('fetched bytes are stored once, named by their content', async () => {
  const bytes = gradientPng(40, 40)
  const a = await storeImageBytes(bytes, 'image/png'), b = await storeImageBytes(bytes, 'image/png')
  expect(a).toBe(b)
  expect(existsSync(a)).toBe(true)
})

// ADE-122: a disk cleanup deleted the cache folder while the app ran, and every fetched image failed to show.
test('a cache folder deleted between two stores is made again', async () => {
  const first = await storeImageBytes(gradientPng(12, 12), 'image/png')
  const folder = path.dirname(first)
  rmSync(folder, { recursive: true, force: true })
  const second = await storeImageBytes(gradientPng(14, 14), 'image/png')
  expect(path.dirname(second)).toBe(folder)
  expect(existsSync(second)).toBe(true)
  // The same bytes as before the cleanup are written again, not looked up in the folder that went away.
  rmSync(folder, { recursive: true, force: true })
  expect(await storeImageBytes(gradientPng(12, 12), 'image/png')).toBe(first)
  expect(existsSync(first)).toBe(true)
  // A path for the native layer to write into has its folder.
  rmSync(folder, { recursive: true, force: true })
  expect(existsSync(path.dirname(imageCachePath('visual-frame-test.png')))).toBe(true)
})

test('an unwritable cache folder still gives the image a file to show', async () => {
  const folder = path.dirname(await storeImageBytes(gradientPng(16, 16), 'image/png'))
  rmSync(folder, { recursive: true, force: true })
  // A file where the folder was: it can be neither written into nor made again.
  writeFileSync(folder, 'not a folder')
  try {
    const bytes = gradientPng(18, 18)
    const file = await storeImageBytes(bytes, 'image/png')
    expect(path.dirname(file)).not.toBe(folder)
    expect(readFileSync(file).equals(Buffer.from(bytes))).toBe(true)
    // This process's own temp folder, not a name another user could make first in the shared temp dir.
    expect(path.dirname(file)).toBe(tempImageFolder())
    expect(path.basename(path.dirname(file))).not.toContain(String(process.pid))
  } finally {
    rmSync(folder, { force: true })
  }
})

test('a deleted temp folder is made anew under a new name', () => {
  const first = tempImageFolder()
  expect(tempImageFolder()).toBe(first)
  rmSync(first, { recursive: true, force: true })
  const second = tempImageFolder()
  expect(second).not.toBe(first)
  expect(existsSync(second)).toBe(true)
})
