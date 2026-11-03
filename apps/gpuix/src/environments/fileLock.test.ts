// The lock file every GPUIX window shares around the combined Keychain item (`credentials.ts`), and the move from one
// item per id with several windows starting at once, as separate processes.
import { afterEach, expect, test } from 'bun:test'
import { existsSync, mkdtempSync, readFileSync, rmSync, statSync, utimesSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { fileLock } from './credentials.ts'

let root = ''
afterEach(() => { if (root) rmSync(root, { recursive: true, force: true }); root = '' })
const scratch = () => (root = mkdtempSync(path.join(tmpdir(), 'kybern-lock-')))
const sleep = (ms: number) => new Promise(resolve => setTimeout(resolve, ms))
const fast = { waitMs: 2_000, staleMs: 300, heartbeatMs: 50 }

test('the lock names its holder, which keeps it while its work outlasts the stale time', async () => {
  const lockFile = path.join(scratch(), 'k.lock')
  const lock = fileLock(lockFile, fast)
  let second = false
  const first = lock(async () => {
    expect(readFileSync(lockFile, 'utf8')).toStartWith(`${process.pid}:`)
    // A Keychain dialog: much longer than the stale time. The heartbeat keeps the lock.
    await sleep(fast.staleMs * 3)
    expect(second).toBe(false)
    return 'first'
  })
  await sleep(20)
  const waiter = lock(async () => { second = true; return 'second' })
  expect(await first).toBe('first')
  expect(await waiter).toBe('second')
  expect(existsSync(lockFile)).toBe(false)
})

test('a lock left by a process that is gone, or one nobody touches, is taken over', async () => {
  const lockFile = path.join(scratch(), 'k.lock')
  // No process has this pid (above every pid limit).
  writeFileSync(lockFile, '2147483646:left-behind')
  expect(await fileLock(lockFile, fast)(async () => 'taken')).toBe('taken')
  // An older release's empty lock, untouched for longer than the stale time.
  writeFileSync(lockFile, '')
  const old = new Date(Date.now() - fast.staleMs * 2)
  utimesSync(lockFile, old, old)
  expect(await fileLock(lockFile, fast)(async () => 'taken')).toBe('taken')
  // A live holder's lock that is touched is not.
  writeFileSync(lockFile, `${process.pid}:someone-else`)
  await expect(fileLock(lockFile, { ...fast, waitMs: 100, staleMs: 10_000 })(async () => 'taken')).rejects.toThrow('Another Kybern window')
})

test('a holder removes only its own lock', async () => {
  const lockFile = path.join(scratch(), 'k.lock')
  await fileLock(lockFile, fast)(async () => {
    // Another window took the lock over meanwhile (this one looked stale to it).
    writeFileSync(lockFile, `${process.pid}:another-window`)
  })
  expect(readFileSync(lockFile, 'utf8')).toBe(`${process.pid}:another-window`)
  expect(statSync(lockFile).isFile()).toBe(true)
})

test('windows starting at once read each old Keychain item once, and every window gets its token', async () => {
  const dir = scratch()
  const old: Record<string, string> = { 'machine:1': 't1', 'machine:2': 't2', 'machine:3': 't3' }
  for (const [id, token] of Object.entries(old)) writeFileSync(path.join(dir, `${encodeURIComponent(id)}.item`), token)
  const script = path.join(import.meta.dir, 'keychainMigration.process.ts')
  const ids = Object.keys(old)
  const windows = [0, 1, 2].map(() => Bun.spawn([process.execPath, script, dir, path.join(dir, 'k.lock'), ...ids], { stdout: 'pipe', stderr: 'pipe' }))
  const outputs = await Promise.all(windows.map(async child => {
    const [out, err, code] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited])
    expect(code, err).toBe(0)
    return JSON.parse(out) as string[]
  }))
  for (const read of outputs) expect(read).toEqual(['t1', 't2', 't3'])
  const reads = readFileSync(path.join(dir, 'reads.log'), 'utf8').trim().split('\n').map(line => line.split(' ')[1])
  for (const id of ids) expect(reads.filter(account => account === id)).toHaveLength(1)
  expect(JSON.parse(readFileSync(path.join(dir, 'all.item'), 'utf8'))).toEqual(old)
  for (const id of ids) expect(existsSync(path.join(dir, `${encodeURIComponent(id)}.item`))).toBe(false)
  expect(existsSync(path.join(dir, 'k.lock'))).toBe(false)
}, 30_000)
