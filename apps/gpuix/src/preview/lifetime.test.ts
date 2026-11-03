import { expect, test } from 'bun:test'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import type { PortForward } from '../../../../packages/kybern-client/src/types.ts'
import { FORWARD_GRACE_MS, forwardInUse, loopbackPort, previewUse, watchPreviewLifetime, type UseSource } from './lifetime.ts'

const forward = (id: string, patch: Partial<PortForward> = {}): PortForward => ({ id, machine_id: 'instabox', machine_name: 'instabox', remote_port: 3000, local_port: 3000, connections: 0, created_at: '2026-11-03T10:00:00Z', origin: 'preview', ...patch })
const shown = (port = 3000): UseSource => ({ entries: [{ address: `http://localhost:${port}`, url: `http://localhost:${port}`, machine: { id: 'instabox', port: 3000 } }] })

/** A browser per thread, its tab, the daemon's forwards, and a clock the test moves. */
function world(forwards: PortForward[]) {
  let now = 0
  const timers = new Map<number, { at: number; run: () => void }>()
  let serial = 0
  const browsers = new Map<string, UseSource>(), tabs = new Set<string>(), listeners = new Set<() => void>()
  const stopped: string[] = [], disposed: string[] = []
  const changed = () => { for (const listener of listeners) listener() }
  const stopWatching = watchPreviewLifetime({
    threads: () => [...browsers.keys()],
    open: id => tabs.has(id),
    dispose: id => { disposed.push(id); browsers.delete(id) },
    uses: () => [...browsers.values()].map(previewUse),
    forwards: () => forwards,
    stop: async id => { stopped.push(id); forwards = forwards.filter(f => f.id !== id); changed() },
    subscribe: listener => { listeners.add(listener); return () => { listeners.delete(listener) } },
    now: () => now,
    setTimer: (run, ms) => { timers.set(++serial, { at: now + ms, run }); return serial },
    clearTimer: id => { timers.delete(id as number) },
  })
  /** Run every timer due within `ms`, in order. */
  const advance = (ms: number) => {
    const end = now + ms
    for (;;) {
      const next = [...timers].filter(([, t]) => t.at <= end).sort((a, b) => a[1].at - b[1].at)[0]
      if (!next) break
      timers.delete(next[0]); now = Math.max(now, next[1].at); next[1].run()
    }
    now = end
  }
  return {
    open(id: string, source: UseSource = shown()) { tabs.add(id); browsers.set(id, source); changed() },
    close(id: string) { tabs.delete(id); changed() },
    setForwards(next: PortForward[]) { forwards = next; changed() },
    advance, stopped, disposed, browsers, stopWatching,
  }
}

test('a preview forward stops 10 s after its last browser tab closes, and the page goes at once', () => {
  const w = world([forward('f1')])
  w.open('thread-a'); w.advance(0)
  w.close('thread-a'); w.advance(0)
  expect(w.disposed).toEqual(['thread-a'])
  w.advance(FORWARD_GRACE_MS - 1)
  expect(w.stopped).toEqual([])
  w.advance(1)
  expect(w.stopped).toEqual(['f1'])
})

test('a forward is kept while another thread still shows it, and stops after that one closes too', () => {
  const w = world([forward('f1')])
  w.open('thread-a'); w.open('thread-b'); w.advance(0)
  w.close('thread-a'); w.advance(FORWARD_GRACE_MS * 2)
  expect(w.stopped).toEqual([])
  w.close('thread-b'); w.advance(FORWARD_GRACE_MS)
  expect(w.stopped).toEqual(['f1'])
})

test('a browser that shows the forward again within the grace period keeps it', () => {
  const w = world([forward('f1')])
  w.open('thread-a'); w.advance(0)
  w.close('thread-a'); w.advance(4000)
  // A new tab opens the same server: while it opens, the server it is opening already counts.
  w.open('thread-a', { entries: [], attemptedMachine: { id: 'instabox', port: 3000 } }); w.advance(FORWARD_GRACE_MS * 3)
  expect(w.stopped).toEqual([])
  w.close('thread-a'); w.advance(FORWARD_GRACE_MS)
  expect(w.stopped).toEqual(['f1'])
})

test('a tab moved between groups in one update never reads as closed', () => {
  const w = world([forward('f1')])
  w.open('thread-a'); w.advance(0)
  // Taken out of one group and put in another before the next tick.
  w.close('thread-a'); w.open('thread-a', w.browsers.get('thread-a')); w.advance(FORWARD_GRACE_MS * 2)
  expect(w.disposed).toEqual([])
  expect(w.stopped).toEqual([])
})

test('a manual forward is never stopped, even after a preview showed it', () => {
  const w = world([forward('f1', { origin: undefined })])
  w.open('thread-a'); w.advance(0)
  w.close('thread-a'); w.advance(FORWARD_GRACE_MS * 3)
  expect(w.disposed).toEqual(['thread-a'])
  expect(w.stopped).toEqual([])
  const explicit = world([forward('f2', { origin: 'manual' })])
  explicit.open('thread-a'); explicit.advance(0); explicit.close('thread-a'); explicit.advance(FORWARD_GRACE_MS * 3)
  expect(explicit.stopped).toEqual([])
})

test('a preview forward made manual while its tab was open is kept', () => {
  const w = world([forward('f1')])
  w.open('thread-a'); w.advance(0)
  w.setForwards([forward('f1', { origin: undefined })])
  w.close('thread-a'); w.advance(FORWARD_GRACE_MS * 3)
  expect(w.stopped).toEqual([])
})

test("an older daemon's forwards (no origin) are never stopped", () => {
  const { origin: _origin, ...old } = forward('f1')
  const w = world([old as PortForward])
  w.open('thread-a'); w.advance(0)
  w.close('thread-a'); w.advance(FORWARD_GRACE_MS * 3)
  expect(w.stopped).toEqual([])
})

test('a preview forward no browser of this app showed is left alone', () => {
  // Another client's preview, or one opened before this app started.
  const w = world([forward('f1', { remote_port: 5173, local_port: 5173 })])
  w.open('thread-a'); w.advance(0)
  w.close('thread-a'); w.advance(FORWARD_GRACE_MS * 3)
  expect(w.stopped).toEqual([])
})

test('the agent opens a forward by its local address, and Fast mode loads its HTTPS port', () => {
  const agent = forward('f1', { local_port: 41000 })
  expect(forwardInUse(agent, [previewUse({ entries: [{ address: 'http://localhost:41000/app', url: 'http://localhost:41000/app' }] })])).toBe(true)
  const fast = forward('f2', { local_port: 41001, https_port: 41002 })
  expect(forwardInUse(fast, [previewUse({ entries: [{ address: 'http://localhost:9', url: 'https://localhost:41002/' }] })])).toBe(true)
  expect(forwardInUse(fast, [previewUse({ entries: [{ address: 'https://example.com', url: 'https://example.com' }] })])).toBe(false)
  expect(loopbackPort('http://127.0.0.1:41000')).toBe(41000)
  expect(loopbackPort('/Users/me/index.html')).toBeNull()
  // A page left within the grace period stops once the agent's preview tab closes.
  const w = world([agent])
  w.open('thread-a', { entries: [{ address: 'http://localhost:41000/app', url: 'http://localhost:41000/app' }] }); w.advance(0)
  w.close('thread-a'); w.advance(FORWARD_GRACE_MS)
  expect(w.stopped).toEqual(['f1'])
})

test('stopping the watcher cancels a pending stop', () => {
  const w = world([forward('f1')])
  w.open('thread-a'); w.advance(0)
  w.close('thread-a'); w.advance(1000)
  w.stopWatching(); w.advance(FORWARD_GRACE_MS * 2)
  expect(w.stopped).toEqual([])
})

test('a closed browser tab leaves nothing for the next one: page, history, relay, ticket and Fast choice', async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'gpuix-scratch-preview-lifetime-'))
  try {
    const child = Bun.spawn(['bun', path.join(import.meta.dir, 'lifetime.verify.ts')], { env: { ...process.env, KYBERN_DATA_DIR: dir, KYBERN_CLIENT_CONFIG_DIR: path.join(dir, 'config') }, stdout: 'pipe', stderr: 'pipe' })
    const output = await new Response(child.stderr).text()
    expect(await child.exited, output).toBe(0)
  } finally { rmSync(dir, { recursive: true, force: true }) }
})
