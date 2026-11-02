// App-wide preferences that belong to this installation rather than to one
// environment (the desktop keeps these in its own localStorage): the dismissed
// update announcement, for example. One small JSON file beside the GPUIX
// appearance files; `KYBERN_CLIENT_CONFIG_DIR` moves it for tests.
import { mkdirSync, readFileSync, renameSync, writeFileSync } from 'node:fs'
import path from 'node:path'
import { gpuixConfigDir } from './os.ts'

const file = () => path.join(gpuixConfigDir(), 'app.json')
let cache: Record<string, unknown> | null = null
function all(): Record<string, unknown> {
  if (cache) return cache
  try { cache = JSON.parse(readFileSync(file(), 'utf8')) as Record<string, unknown> } catch { cache = {} }
  return cache
}
export const readPref = <T>(key: string, fallback: T): T => (key in all() ? all()[key] as T : fallback)
/**
 * Read the file again, for a value another window's process may have written since (each environment window is its own
 * process). Unlike `readPref` it throws when the file exists but cannot be read or parsed: a caller that must be able to
 * keep what it writes (a dismissal) can then refuse to go on.
 */
export function reloadPrefs(): Record<string, unknown> {
  let next: Record<string, unknown>
  try {
    const parsed: unknown = JSON.parse(readFileSync(file(), 'utf8'))
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) throw new Error('app.json is not an object')
    next = parsed as Record<string, unknown>
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error
    next = {}
  }
  cache = next
  return next
}
export function writePref(key: string, value: unknown) {
  const next = { ...all(), [key]: value }
  mkdirSync(path.dirname(file()), { recursive: true })
  const temp = `${file()}.${process.pid}.tmp`
  writeFileSync(temp, JSON.stringify(next))
  renameSync(temp, file())
  cache = next
}
