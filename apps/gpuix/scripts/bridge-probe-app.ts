/**
 * The real app plus one harness endpoint (WP1): scroll the transcript from inside the app process and report the
 * UI-thread bridge cost (`getBridgeStats`) of that window. `platform-smoke.ts --scroll` launches this instead of
 * `src/main.tsx`; the rendered product is unchanged.
 *
 * The endpoint listens on 127.0.0.1 (port written to `$KYBERN_DATA_DIR/bridge-probe.port`) and takes
 * `POST {"action":"scroll","target":"transcript","durationMs":2000}`: wheel steps every 16 ms over the target's centre,
 * up for the first half and back down for the second, with the bridge counters read before and after and the scroll
 * offset at the start, the turn and the end.
 * The wheel input is itself a waited bridge call off macOS; it is reported as `inputCalls` so callers can subtract it.
 *
 * `POST {"action":"type","text":"…","intervalMs":120,"stream":true}` types `text` into the focused composer one key at
 * a time (native key input, the same path a real key takes) and times each key from its input until the composer paints
 * it; `stream` first starts a synthetic agent answer on the open thread (production `applyEvent`, 25 deltas/s, as
 * perf/bench does). It reports keystroke-to-paint latency, the app's waited bridge calls per key and drawn frames.
 * `POST {"action":"stall-key","key":"c","stallMs":4500}` sends one intercepted key and, before JS can answer it, holds
 * the UI thread busy (`stallUiThread`) for longer than the bridge's 2 s timeout: the answer must not throw (an uncaught
 * error is the 0.21.5 "Timed out after 2 seconds waiting for a window update" overlay) and the key must still land.
 */
import { writeFileSync } from 'node:fs'
import path from 'node:path'

type Stats = { bridged: boolean; sent: number; waited: number; waitMs: number; servedFrames: number | null; drawnFrames?: number | null; callbackFrames?: number | null; frameWakes: number }
type Renderer = {
  getAutomationTree(): string
  getElementBounds(id: number): { x: number; y: number; width: number; height: number } | null
  getListScrollTop(id: number): number[] | null
  getScrollOffset(id: number): number[] | null
  simulateScrollWheel(x: number, y: number, deltaX: number, deltaY: number, modifiers?: string | null): void
  simulateKeystrokes(keystrokes: string): void
  focusElement(id: number): void
  getPaintedText(): string[]
  getFocusedElementId?(): number | null
  stallUiThread?(ms: number): void
  getBridgeStats?(): Stats
}

// Errors that reached the process: the app shows each as the "Uncaught runtime errors" overlay.
const uncaught: string[] = []
process.on('uncaughtException', (error) => { uncaught.push(error instanceof Error ? error.message : String(error)) })

const keystroke = (char: string) => char === ' ' ? 'space' : /[A-Z]/.test(char) ? `shift-${char.toLowerCase()}` : char
const quantile = (values: number[], q: number) => {
  if (!values.length) return null
  const sorted = [...values].sort((a, b) => a - b)
  return sorted[Math.min(sorted.length - 1, Math.floor(q * sorted.length))]!
}
let paintedNow = (renderer: Renderer) => renderer.getPaintedText()
/** Wait until a painted line holds `text`; each read is one waited call off macOS. */
async function paintedWith(renderer: Renderer, text: string, timeoutMs: number) {
  const started = performance.now()
  let reads = 0
  while (performance.now() - started < timeoutMs) {
    reads++
    if (paintedNow(renderer).some(line => line.includes(text))) return { ms: performance.now() - started, reads }
    await Bun.sleep(4)
  }
  return { ms: null, reads }
}
let streamTimer: ReturnType<typeof setInterval> | undefined
async function startStream(durationMs: number) {
  const state = await import('../src/state.ts')
  const { applyEvent } = await import('../../../packages/kybern-client/src/transcript.ts')
  const id = state.getState().selectedId
  if (!id) throw new Error('no thread is open to stream into')
  let seq = 9_000_000, n = 0
  const started = performance.now(), turn = `probe-turn-${Date.now()}`, message = `probe-answer-${Date.now()}`
  const push = (payload: Record<string, unknown>) => state.setThreadOpen(id, applyEvent(state.getState().open, { thread_id: id, turn_id: turn, seq: ++seq, at: '2026-11-03T00:00:00Z', ...payload } as never), payload.kind === 'assistant_text_delta')
  push({ kind: 'turn_started', message_id: `${message}-user`, message: { parts: [{ type: 'text', text: 'Write a long answer while I type.' }] } })
  clearInterval(streamTimer)
  streamTimer = setInterval(() => {
    if (performance.now() - started > durationMs) {
      clearInterval(streamTimer)
      push({ kind: 'assistant_message_completed', message_id: message, origin: { kind: 'root' }, text: Array.from({ length: n }, (_, i) => `Streaming paragraph ${i}: preserve **formatted output** and Unicode 🌿.\n\n`).join(''), thinking: null })
      push({ kind: 'turn_completed', stop_reason: 'completed', usage: { input_tokens: 1, output_tokens: 1 }, duration_ms: Math.round(durationMs), terminal_message_id: message })
      return
    }
    push({ kind: 'assistant_text_delta', message_id: message, origin: { kind: 'root' }, delta: `Streaming paragraph ${n++}: preserve **formatted output** and Unicode 🌿.\n\n` })
  }, 40)
}

let root: { containerInfo?: { nativeRenderer?: Renderer } } | undefined
;(globalThis as Record<string, unknown>).__REACT_DEVTOOLS_GLOBAL_HOOK__ = {
  supportsFiber: true, renderers: new Map(), inject() { return 1 },
  onCommitFiberRoot(_id: number, value: typeof root) { root = value }, onCommitFiberUnmount() {}, onScheduleFiberRoot() {},
}

type Node = { id: number; testId?: string; children?: Node[] }
function find(renderer: Renderer, testId: string): number | undefined {
  const tree = JSON.parse(renderer.getAutomationTree()) as Node | Node[] | null
  const stack = Array.isArray(tree) ? [...tree] : tree ? [tree] : []
  for (let visited = 0; stack.length && visited < 200_000; visited++) {
    const node = stack.pop()!
    if (node.testId === testId) return node.id
    stack.push(...(node.children ?? []))
  }
}

const server = Bun.serve({
  port: 0, hostname: '127.0.0.1', idleTimeout: 255,
  async fetch(request) {
    const body = await request.json() as { action: string; target?: string; durationMs?: number; delta?: number }
    const renderer = root?.containerInfo?.nativeRenderer
    if (!renderer) return Response.json({ error: 'the app has not committed yet' }, { status: 503 })
    if (body.action === 'type' || body.action === 'stall-key') return Response.json(await typeKeys(renderer, body as TypeRequest))
    if (body.action !== 'scroll') return Response.json({ error: `unknown action ${body.action}` }, { status: 400 })
    const id = find(renderer, body.target ?? 'transcript')
    const bounds = id == null ? null : renderer.getElementBounds(id)
    if (id == null || !bounds) return Response.json({ error: `no painted ${body.target ?? 'transcript'}` }, { status: 404 })
    const offset = () => renderer.getListScrollTop(id) ?? renderer.getScrollOffset(id)
    const x = bounds.x + bounds.width / 2, y = bounds.y + bounds.height / 2
    const duration = body.durationMs ?? 2000, delta = body.delta ?? 60
    const offsets: (number[] | null)[] = [offset()]
    // Which addon methods the app calls during the window (any of them may be a waited UI-thread call off macOS).
    const calls: Record<string, number> = {}
    const proto = Object.getPrototypeOf(renderer) as Record<string, unknown>
    const wrapped: [string, unknown][] = []
    for (const name of Object.getOwnPropertyNames(proto)) {
      const original = proto[name]
      if (typeof original !== 'function' || name === 'constructor' || name === 'getBridgeStats') continue
      wrapped.push([name, original])
      proto[name] = function (this: unknown, ...args: unknown[]) { calls[name] = (calls[name] ?? 0) + 1; return (original as (...a: unknown[]) => unknown).apply(this, args) }
    }
    const before = renderer.getBridgeStats?.() ?? null
    const started = performance.now()
    // Every call this loop makes into the addon (wheel steps and the one offset read in the middle) waits off macOS.
    let inputCalls = 0, turned = false
    while (performance.now() - started < duration) {
      const elapsed = performance.now() - started
      if (!turned && elapsed >= duration / 2) { offsets.push(offset()); inputCalls++; turned = true }
      // Up (towards older turns) first: the transcript opens at its end.
      renderer.simulateScrollWheel(x, y, 0, elapsed < duration / 2 ? delta : -delta)
      inputCalls++
      await Bun.sleep(16)
    }
    const after = renderer.getBridgeStats?.() ?? null
    for (const [name, original] of wrapped) proto[name] = original
    offsets.push(offset())
    return Response.json({ seconds: (performance.now() - started) / 1000, inputCalls, before, after, offsets, calls })
  },
})
type TypeRequest = { action: 'type' | 'stall-key'; text?: string; intervalMs?: number; stream?: boolean; streamMs?: number; key?: string; stallMs?: number; target?: string }
async function typeKeys(renderer: Renderer, body: TypeRequest) {
  const id = find(renderer, body.target ?? 'composer')
  if (id == null) return { error: `no ${body.target ?? 'composer'} in the tree` }
  renderer.focusElement(id)
  await Bun.sleep(300)
  if (body.stream) { await startStream(body.streamMs ?? 60_000); await Bun.sleep(1500) }
  const errorsBefore = uncaught.length
  paintedNow = (renderer: Renderer) => own(() => renderer.getPaintedText())
  const marker = `k${Date.now() % 100000}`
  // A unique prefix first, so the painted text can only match this run's input.
  const text = body.action === 'stall-key' ? (body.key ?? 'c') : body.text ?? 'the quick brown fox jumps over the lazy dog'
  // Addon calls by method, with the time spent in them, excluding the probe's own input and paint reads.
  const calls: Record<string, { count: number; ms: number }> = {}
  const proto = Object.getPrototypeOf(renderer) as Record<string, unknown>
  const wrapped: [string, unknown][] = []
  let probing = false
  for (const name of Object.getOwnPropertyNames(proto)) {
    const original = proto[name]
    if (typeof original !== 'function' || name === 'constructor' || name === 'getBridgeStats') continue
    wrapped.push([name, original])
    proto[name] = function (this: unknown, ...args: unknown[]) {
      if (probing) return (original as (...a: unknown[]) => unknown).apply(this, args)
      const at = performance.now()
      try { return (original as (...a: unknown[]) => unknown).apply(this, args) }
      finally { const entry = calls[name] ??= { count: 0, ms: 0 }; entry.count++; entry.ms += performance.now() - at }
    }
  }
  const own = <T,>(run: () => T) => { probing = true; try { return run() } finally { probing = false } }
  const before = renderer.getBridgeStats?.() ?? null
  const started = performance.now()
  let probeCalls = 0
  const latencies: number[] = [], missed: string[] = []
  for (const char of `${marker} `) { own(() => renderer.simulateKeystrokes(keystroke(char))); probeCalls++ }
  const typed0 = await paintedWith(renderer, marker, 10_000); probeCalls += typed0.reads
  if (typed0.ms == null) for (const [name, original] of wrapped) proto[name] = original
  if (typed0.ms == null) return { error: `the marker ${marker} never painted in the ${body.target ?? 'composer'}`, uncaught: uncaught.slice(errorsBefore), composer: id, focused: renderer.getFocusedElementId?.() ?? null, painted: renderer.getPaintedText().slice(-30) }
  const keysStarted = performance.now(), statsKeys = renderer.getBridgeStats?.() ?? null
  probeCalls = 0
  for (const name of Object.keys(calls)) delete calls[name]
  let typed = `${marker} `
  for (const char of text) {
    const sent = performance.now()
    // One waited call: GPUI dispatches the key (and its text) before it returns.
    own(() => renderer.simulateKeystrokes(keystroke(char)))
    // An intercepted key now waits on the UI thread for JS's answer: stall the UI thread before JS can give it.
    if (body.action === 'stall-key') own(() => renderer.stallUiThread?.(body.stallMs ?? 4500))
    probeCalls++
    typed += char
    const shown = await paintedWith(renderer, typed.trimEnd(), body.action === 'stall-key' ? 15_000 : 10_000)
    probeCalls += shown.reads
    if (shown.ms == null) missed.push(char)
    else latencies.push(performance.now() - sent)
    const rest = (body.intervalMs ?? 120) - (performance.now() - sent)
    if (rest > 0) await Bun.sleep(rest)
  }
  const after = renderer.getBridgeStats?.() ?? null
  for (const [name, original] of wrapped) proto[name] = original
  paintedNow = (renderer: Renderer) => renderer.getPaintedText()
  clearInterval(streamTimer)
  const keys = text.length
  const waited = statsKeys && after ? after.waited - statsKeys.waited : null
  const appWaited = waited == null ? null : Math.max(0, waited - probeCalls)
  return {
    keys, missed, seconds: (performance.now() - keysStarted) / 1000,
    latencyMs: { p50: quantile(latencies, 0.5), p90: quantile(latencies, 0.9), max: quantile(latencies, 1), mean: latencies.length ? latencies.reduce((a, b) => a + b, 0) / latencies.length : null },
    appWaitedPerKey: appWaited == null ? null : appWaited / keys,
    waitMsPerKey: statsKeys && after ? (after.waitMs - statsKeys.waitMs) / keys : null,
    drawnFramesPerKey: statsKeys?.drawnFrames != null && after?.drawnFrames != null ? (after.drawnFrames - statsKeys.drawnFrames) / keys : null,
    appCalls: Object.fromEntries(Object.entries(calls).sort((a, b) => b[1].ms - a[1].ms).map(([name, entry]) => [name, { perKey: +(entry.count / keys).toFixed(2), msPerKey: +(entry.ms / keys).toFixed(2) }])),
    before, after, uncaught: uncaught.slice(errorsBefore), totalSeconds: (performance.now() - started) / 1000,
  }
}

writeFileSync(path.join(process.env.KYBERN_DATA_DIR!, 'bridge-probe.port'), String(server.port))
await import('../src/main.tsx')
