/**
 * Platform smoke test (ADE-52, extended by WP7): a scratch daemon with one seeded
 * thread, then the app and the rounded-clip lab on screen, each captured to a PNG
 * and asserted.
 *
 *   bun scripts/platform-smoke.ts --kybernd <path> --out <dir> [--data <scratch dir>] [--port 4377] [--only app|lab] [--capture none] [--scroll 0]
 *
 * Same seeding technique as perf/bench/seed.ts (RPC creates the records, SQL
 * appends synthetic provider events), without its macOS helpers; no agent runs.
 * Captures and the launch go through `native/check-harness.ts` (the addon's
 * screenshot, or the X server on Linux when the addon cannot). Assertions tolerate
 * software rendering: structure (test ids, bounds, painted text) first, then
 * colours within `tolerance` and regions that must hold painted pixels.
 *
 * App: the sidebar lists the three threads, the transcript paints the Markdown
 * answer (heading, table, inline code, the highlighted block) and the composer is
 * on screen, with a dark backdrop. Lab: the quad tile's centre is red and its
 * corner is the frame colour (rounded) or, off macOS, red with `KYBERN_ROUNDED_CLIP=0` (the square
 * "before" look), so the two runs prove the clip switch on the platform's shader.
 *
 * Under a headless Wayland compositor (WP1) there is nothing to grab: `--capture none`
 * still requires both windows to come up and the seeded message to be painted
 * (automation tree), and writes no PNGs or pixel assertions.
 *
 * Transcript scroll (WP1, `--scroll 0` skips it): the app again, through
 * `scripts/bridge-probe-app.ts`, on a third thread seeded with 24 long turns. It wheels
 * the transcript up and back for 2 s and reads `getBridgeStats` around it: off
 * macOS the app's own waited UI-thread calls (the wheel input subtracted) must stay
 * under 0.5 per served frame, and the transcript must move. Writes `scroll.json`.
 *
 * Typing (`--typing 0` skips it): in the same window, the composer of that thread gets a sentence one key at a time
 * while a synthetic answer streams, timed from each key's input to its paint; every key must land and no error may
 * reach the process. Off macOS one intercepted key is then answered while the UI thread is held busy (4.5 s) past the
 * bridge's 2 s timeout (0.21.5 threw "Timed out after 2 seconds waiting for a window update" from resolveKeyEvent):
 * it must land without an error. Writes `typing.json`.
 */
import { spawn } from 'node:child_process'
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import path from 'node:path'
import { Database } from 'bun:sqlite'
import { assertPainted, capture, launch, near, platform, pngPixels, step, tolerance, type Fixture } from '../native/check-harness.ts'
import { waitForPainted } from '../src/requests/harness.ts'
import assert from 'node:assert/strict'
import { KybernClient } from '../../../packages/kybern-client/src/client.ts'

const argv = process.argv.slice(2)
const arg = (name: string, fallback?: string) => {
  const at = argv.indexOf(`--${name}`)
  const value = at >= 0 ? argv[at + 1] : fallback
  if (value === undefined) throw new Error(`--${name} is required`)
  return value
}
const out = path.resolve(arg('out'))
const kybernd = path.resolve(arg('kybernd'))
const port = Number(arg('port', '4377'))
const settle = Number(arg('settle-ms', '4000'))
const captureMode = arg('capture', 'png')
const scrollScenario = arg('scroll', '1') !== '0'
const typingScenario = arg('typing', '1') !== '0'
const SCROLL_TURNS = 24
const os = process.platform === 'win32' ? 'windows' : process.platform
// The scratch daemon's data. Keep it out of `--out` when that folder is uploaded: it holds sockets.
const data = path.resolve(arg('data', path.join(out, 'scratch')))
rmSync(data, { recursive: true, force: true })
mkdirSync(path.join(data, 'project'), { recursive: true })

const daemon = spawn(kybernd, ['--data-dir', data, '--port', String(port)], { stdio: 'inherit' })
const stop = () => daemon.exitCode === null && daemon.kill()
process.on('exit', stop)
const deadline = Date.now() + 60_000
while (!existsSync(path.join(data, 'daemon.port'))) {
  if (daemon.exitCode !== null || Date.now() > deadline) throw new Error('The scratch daemon did not start')
  await Bun.sleep(50)
}
const token = readFileSync(path.join(data, 'daemon.token'), 'utf8').trim()
const client = new KybernClient(
  { url: `ws://127.0.0.1:${port}/ws`, token },
  { createSocket: (url, auth) => new WebSocket(url, { headers: { origin: 'tauri://localhost', Authorization: `Bearer ${auth}` } } as unknown as string[]) },
)
await new Promise<void>((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error('Connection timeout')), 15_000)
  client.onStatus((status) => status === 'open' && (clearTimeout(timer), resolve()))
  client.connect()
})
const call = client.call.bind(client) as (method: string, params: unknown) => Promise<any>
const project = await call('projects.add', { path: path.join(data, 'project'), name: 'Platform smoke' })
const provider = { kind: 'claude-code', instance: 'default' }
const threads: string[] = []
for (const title of ['Render the transcript on ' + os, 'Rounded clipping in WGSL and HLSL', 'Bun main thread or the UI thread']) {
  threads.push((await call('threads.create', { project_id: project.id, title, provider, model: 'claude-haiku-5-5' })).id)
}
client.close()

const answer = `## It draws on ${os}

The patched addon builds for this platform, and GPUI paints the **sidebar**, this *transcript* and the composer.

| Piece | Status |
| --- | --- |
| Rounded clip | ported |
| Backdrop blur | translucent fill |

- Markdown with \`inline code\`
- A fenced block below

\`\`\`rust
fn update_window<R: Send + 'static>(update: impl FnOnce(&mut View) -> R + Send + 'static) -> Result<R> {
    run_on_ui_thread(move |window, cx| window.update(cx, update))
}
\`\`\`
`
const db = new Database(path.join(data, 'state.sqlite'))
try {
  const insert = db.query('INSERT INTO events(thread_id,turn_id,at,kind,payload) VALUES (?,?,?,?,?)')
  const at = '2026-10-22T00:00:00Z'
  const id = (n: number) => `00000000-0000-4000-a000-${String(n).padStart(12, '0')}`
  // The scroll thread: SCROLL_TURNS turns of the same answer, ids from 1000 up.
  const scrollEvents = Array.from({ length: SCROLL_TURNS }, (_, turn) => {
    const base = 1000 + turn * 10
    return { turn: id(base), events: [
      { kind: 'turn_started', message_id: id(base + 1), message: { parts: [{ type: 'text', text: `Scroll turn ${turn + 1}` }] } },
      // Long answers: earlier-history paging shows only the latest turns, which must still overflow the viewport.
      { kind: 'assistant_message_completed', message_id: id(base + 2), origin: { kind: 'root' }, text: `${Array.from({ length: 6 }, () => answer).join('\n\n')}\n\nEnd of scroll turn ${turn + 1}.`, thinking: null },
      { kind: 'turn_completed', stop_reason: 'completed', usage: { input_tokens: 10, output_tokens: 5, cache_read_tokens: 0, cache_write_tokens: 0 }, cost_usd: null, duration_ms: 1000, terminal_message_id: id(base + 2) },
    ] }
  })
  const events = [
    { kind: 'turn_started', message_id: id(1), message: { parts: [{ type: 'text', text: `Show me the GPUIX client running on ${os}.` }] } },
    { kind: 'tool_call_started', call: { id: id(2), name: 'Read', input: { file_path: 'apps/gpuix/PORTING-PLATFORMS.md' } }, origin: { kind: 'root' } },
    { kind: 'tool_call_completed', tool_call_id: id(2), output: { text: '# Porting GPUIX' }, is_error: false },
    { kind: 'assistant_message_completed', message_id: id(3), origin: { kind: 'root' }, text: answer, thinking: null },
    { kind: 'turn_completed', stop_reason: 'completed', usage: { input_tokens: 1000, output_tokens: 500, cache_read_tokens: 0, cache_write_tokens: 0 }, cost_usd: null, duration_ms: 4000, terminal_message_id: id(3) },
  ]
  db.transaction(() => {
    for (const event of events) insert.run(threads[0]!, id(10), at, event.kind, JSON.stringify(event))
    for (const { turn, events } of scrollEvents) for (const event of events) insert.run(threads[2]!, turn, at, event.kind, JSON.stringify(event))
    for (const thread of [threads[0]!, threads[2]!]) db.query('UPDATE threads SET last_seq=(SELECT MAX(seq) FROM events WHERE thread_id=?),updated_at=? WHERE id=?').run(thread, at, thread)
  })()
} finally {
  db.close()
}

const painted = async (f: Fixture) => (await f.app.call('getPaintedText', {})).text.join('\n')

async function shoot(entry: string, ready: string, file: string, env: Record<string, string>, verify: (f: Fixture, png: ReturnType<typeof pngPixels>) => Promise<void>, expectText?: string) {
  const f = await launch(entry, { KYBERN_APPEARANCE: 'dark', ...env })
  try {
    await f.app.getByTestId(ready).waitFor({ timeoutMs: 90_000 })
    // A software-rendered first frame can starve the app's 750 ms daemon probe on a
    // small runner; the app then offers Reconnect, which is what a user would press.
    let shown = !expectText
    for (let attempt = 0; expectText && attempt < 4; attempt++) {
      try {
        await waitForPainted(f.app, expectText, 20_000)
        shown = true
        break
      } catch {
        console.log(`[smoke] "${expectText}" not on screen yet; pressing Reconnect`)
        await f.app.getByText('Reconnect').click().catch(() => {})
      }
    }
    if (captureMode === 'none') {
      // Without a PNG the painted text is the only evidence the transcript drew.
      if (!shown) throw new Error(`"${expectText}" never appeared on screen`)
      console.log(`[smoke] on screen (no capture): ${file}`)
      return
    }
    await Bun.sleep(settle)
    const shot = await capture(f, path.join(out, file))
    console.log(`[smoke] wrote ${shot}`)
    await verify(f, pngPixels(shot))
  } catch (error) {
    console.error(f.diagnostics())
    throw error
  } finally {
    await f.close().catch(() => {})
  }
}

const only = argv.includes('--only') ? argv[argv.indexOf('--only') + 1] : undefined
const roundedClip = process.env.KYBERN_ROUNDED_CLIP !== '0'
const RED = [0xe8, 0x4c, 0x42]

const verifyLab = async (f: Fixture, png: ReturnType<typeof pngPixels>) => {
  const scale = png.width / (await f.app.getByTestId('ready').bounds()).width
  const reference = await f.app.getByTestId('frame-reference').bounds()
  const frame = png.at((reference.x + 5) * scale, (reference.y + 5) * scale)
  const quad = await f.app.getByTestId('quad').bounds()
  await step('lab: the quad tile paints', () => assert.ok(near(png.at((quad.x + quad.width / 2) * scale, (quad.y + quad.height / 2) * scale), RED), 'the quad tile centre is red'))
  await step(`lab: the quad corner is ${roundedClip ? 'rounded away' : 'square (clip off)'}`, () => {
    const corner = png.at((quad.x + 1) * scale, (quad.y + 1) * scale)
    if (roundedClip) assert.ok(near(corner, frame), `rounded clip: the corner shows the frame colour, got ${corner}`)
    // The switch lives in the wgpu and Direct3D renderers; the macOS addon has no such variable, so there is no "before" look to assert.
    else if (platform === 'macos') console.log('[smoke] KYBERN_ROUNDED_CLIP=0 has no effect on macOS; the square corner is not asserted')
    else assert.ok(near(corner, RED), `KYBERN_ROUNDED_CLIP=0: the corner is square and red, got ${corner}`)
  })
}

const verifyApp = async (f: Fixture, png: ReturnType<typeof pngPixels>) => {
  const root = await f.app.getByTestId('app-ready').bounds()
  const scale = png.width / root.width
  await step('app: the sidebar lists the seeded threads', async () => {
    assert.equal(await f.app.getByTestId('thread-list').count(), 1, 'the thread list')
    for (const id of threads) assert.equal(await f.app.getByTestId(`thread-${id}`).count(), 1, `thread row ${id}`)
    const list = await f.app.getByTestId('thread-list').bounds()
    assert.ok(list.width > 120 && list.height > 80, `thread list is ${list.width}x${list.height}`)
    assertPainted(png, scale, list, 'sidebar')
  })
  await step('app: the transcript paints Markdown and code', async () => {
    const text = await painted(f)
    for (const piece of [`It draws on ${os}`, 'Rounded clip', 'Backdrop blur', 'inline code', 'update_window']) assert.ok(text.includes(piece), `painted text lacks "${piece}"`)
    const sidebar = await f.app.getByTestId('thread-list').bounds()
    assertPainted(png, scale, { x: sidebar.x + sidebar.width + 24, y: root.y + 60, width: root.width - sidebar.width - 48, height: Math.min(300, root.height - 260) }, 'transcript')
  })
  await step('app: the composer is on screen', async () => {
    const composer = await f.app.getByTestId('composer').bounds()
    assert.ok(composer.width > 200 && composer.height > 20, `composer is ${composer.width}x${composer.height}`)
    assert.ok(composer.y + composer.height <= root.y + root.height + 1 && composer.x >= root.x, 'the composer sits inside the window')
    assertPainted(png, scale, composer, 'composer', 4)
  })
  await step('app: dark appearance', () => {
    const corner = png.at(root.x * scale + 3, (root.y + root.height / 2) * scale)
    assert.ok(Math.max(...corner) < 90 + tolerance, `the window edge is dark, got ${corner}`)
  })
}

const app = path.join(import.meta.dir, '../src/main.tsx')
const scratchEnv = {
  KYBERN_DATA_DIR: data,
  KYBERND: kybernd,
  KYBERN_CLIENT_CONFIG_DIR: path.join(data, 'client'),
  KYBERN_GPUIX_CREDENTIALS_DIR: path.join(data, 'credentials'),
  KYBERN_GPUIX_ENVIRONMENT: 'local',
  KYBERN_URL: '',
  KYBERN_TOKEN: '',
  KYBERN_GPUIX_THREAD: threads[0]!,
}
/** Each GPUI draw of the probe window (`KYBERN_FRAME_COSTS`), as the app's stderr reports it. */
const draws: { at: number; renderMs: number; layoutMs: number; layoutCalls: number }[] = []
let drawRemainder = ''
function drawStats(from: number, to: number) {
  const within = draws.filter(draw => draw.at >= from && draw.at <= to)
  const pick = (values: number[], q: number) => values.length ? [...values].sort((a, b) => a - b)[Math.min(values.length - 1, Math.floor(q * values.length))]! : null
  const render = within.map(draw => draw.renderMs), calls = within.map(draw => draw.layoutCalls)
  return { draws: within.length, renderMsTotal: render.reduce((a, b) => a + b, 0), renderMs: { p50: pick(render, 0.5), p90: pick(render, 0.9), max: pick(render, 1) }, layoutCalls: { p50: pick(calls, 0.5), p90: pick(calls, 0.9), max: pick(calls, 1) } }
}
/** WP1: the app's waited bridge calls per frame while the transcript scrolls (`scripts/bridge-probe-app.ts`). */
async function scrollBridge() {
  // KYBERN_FRAME_COSTS: GPUI prints each draw's render and layout time, which the typing probe reports.
  const f = await launch(path.join(import.meta.dir, 'bridge-probe-app.ts'), { KYBERN_APPEARANCE: 'dark', ...scratchEnv, KYBERN_GPUIX_THREAD: threads[2]!, KYBERN_FRAME_COSTS: '1' }, { stderr: chunk => {
    const lines = (drawRemainder + chunk).split('\n')
    drawRemainder = lines.pop() ?? ''
    for (const line of lines) {
      const match = line.match(/\[kybern-work\] renderMs=([\d.]+) layoutMs=([\d.]+) layoutCalls=(\d+)/)
      if (match) draws.push({ at: performance.now(), renderMs: Number(match[1]), layoutMs: Number(match[2]), layoutCalls: Number(match[3]) })
    }
    if (draws.length > 20_000) draws.splice(0, draws.length - 20_000)
  } })
  try {
    await f.app.getByTestId('app-ready').waitFor({ timeoutMs: 90_000 })
    // The app restores the thread the app run left open (KYBERN_GPUIX_THREAD applies only with none): open it as a user would.
    await f.app.getByTestId(`thread-${threads[2]}`).waitFor({ timeoutMs: 60_000 })
    await f.app.getByTestId(`thread-${threads[2]}`).click()
    for (let attempt = 0; ; attempt++) {
      // The tail of the last answer, unique to this thread (followTail opens there).
      try { await waitForPainted(f.app, `End of scroll turn ${SCROLL_TURNS}`, 20_000); break } catch (error) {
        if (attempt >= 3) throw error
        console.log('[smoke] the scroll thread is not on screen yet; pressing Reconnect')
        await f.app.getByText('Reconnect').click().catch(() => {})
      }
    }
    await Bun.sleep(1500)
    const port = readFileSync(path.join(data, 'bridge-probe.port'), 'utf8').trim()
    type Probe = { error?: string; seconds: number; inputCalls: number; offsets: (number[] | null)[]; calls?: Record<string, number>
      before: { bridged: boolean; waited: number; sent: number; waitMs: number; servedFrames: number | null; drawnFrames?: number | null } | null
      after: { bridged: boolean; waited: number; sent: number; waitMs: number; servedFrames: number | null; drawnFrames?: number | null } | null }
    const served = (p: Probe) => p.before?.servedFrames != null && p.after?.servedFrames != null ? p.after.servedFrames - p.before.servedFrames : null
    // Software rendering can draw the full transcript only a couple of times a second (lavapipe with rounded clips on
    // drew 4 frames in 2 s): scroll longer before judging the per-frame cost on too few frames.
    let probe!: Probe
    for (const durationMs of [2000, 6000]) {
      const response = await fetch(`http://127.0.0.1:${port}/`, { method: 'POST', body: JSON.stringify({ action: 'scroll', target: 'transcript', durationMs }) })
      probe = await response.json() as Probe
      if (probe.error) throw new Error(`bridge probe: ${probe.error}`)
      if ((served(probe) ?? 0) >= 5) break
      console.log(`[smoke] ${served(probe)} frames in a ${durationMs} ms scroll; scrolling longer`)
    }
    const { before, after } = probe
    const frames = served(probe)
    const drawn = before?.drawnFrames != null && after?.drawnFrames != null ? after.drawnFrames - before.drawnFrames : null
    const waited = before && after ? after.waited - before.waited : null
    const appWaited = waited == null ? null : Math.max(0, waited - probe.inputCalls)
    const result = {
      bridged: after?.bridged ?? false, seconds: probe.seconds, inputCalls: probe.inputCalls, frames, drawnFrames: drawn,
      sent: before && after ? after.sent - before.sent : null, waited, appWaited,
      appWaitedPerFrame: appWaited == null || !frames ? null : appWaited / frames,
      waitMs: before && after ? after.waitMs - before.waitMs : null, offsets: probe.offsets,
      // Addon calls by method during the window (the probe's own wheel steps and offset read included).
      calls: probe.calls ?? null,
    }
    console.log(`[smoke] transcript scroll bridge: ${JSON.stringify(result)}`)
    writeFileSync(path.join(out, 'scroll.json'), JSON.stringify(result, null, 2))
    await step('scroll: the transcript moves under the wheel', () => {
      // A list's top is [item, offset in it, viewport height] (a plain scroller's is [x, y]): one row up can keep the
      // offset in the row (806/3 → 805/3 when the row is three wheel steps tall), so compare the item too.
      const [start, middle] = probe.offsets.map(offset => offset ? JSON.stringify(offset.slice(0, 2)) : null)
      assert.ok(start != null && middle != null && start !== middle, `the transcript offset did not change (${JSON.stringify(probe.offsets)})`)
    })
    if (result.bridged) await step('scroll: under 0.5 waited bridge calls per frame (wheel input excluded)', () => {
      assert.ok(frames && frames >= 5, `only ${frames} frames served while scrolling: too few to judge per-frame cost`)
      assert.ok(result.appWaitedPerFrame! < 0.5, `${result.appWaitedPerFrame!.toFixed(2)} waited calls per frame (${appWaited} over ${frames} frames)`)
    })
    if (typingScenario) await typing(port)
  } catch (error) {
    console.error(f.diagnostics())
    if (captureMode !== 'none') await capture(f, path.join(out, `${os}-scroll-failure.png`)).catch(() => {})
    throw error
  } finally {
    await f.close().catch(() => {})
  }
}

/** Keys into the composer while an answer streams, then one key answered across a UI-thread stall (`bridge-probe-app.ts`). */
async function typing(port: string) {
  type Typed = { error?: string; keys: number; missed: string[]; latencyMs: Record<string, number | null>; appWaitedPerKey: number | null; drawnFramesPerKey: number | null; uncaught: string[] }
  const post = async (body: Record<string, unknown>) => {
    const typed = await (await fetch(`http://127.0.0.1:${port}/`, { method: 'POST', body: JSON.stringify(body) })).json() as Typed
    if (typed.error) throw new Error(`typing probe: ${typed.error} ${JSON.stringify(typed)}`)
    return typed
  }
  const typingStarted = performance.now()
  const streamed = await post({ action: 'type', text: 'the quick brown fox jumps over the lazy dog', intervalMs: 120, stream: true, streamMs: 30_000 })
  const streamedDraws = drawStats(typingStarted, performance.now())
  console.log(`[smoke] typing while streaming: ${JSON.stringify({ ...streamed, before: undefined, after: undefined })}`)
  console.log(`[smoke] draws while typing: ${JSON.stringify(streamedDraws)}`)
  const report: Record<string, unknown> = { streamed, streamedDraws }
  await step('typing: every key reaches the composer while an answer streams', () => {
    assert.deepEqual(streamed.missed, [], `keys never painted: ${JSON.stringify(streamed.missed)}`)
    assert.deepEqual(streamed.uncaught, [], 'errors reached the process')
  })
  if (platform !== 'macos') {
    // 4.5 s: the key handler's own selection read can wait out 2 s of it, and the answer must still start more than 2 s
    // before the UI thread is free again (0.21.5's waited answer threw there).
    const stalled = await post({ action: 'stall-key', key: 'c', stallMs: 4500 })
    report.stalled = stalled
    console.log(`[smoke] key across a 4.5 s UI stall: ${JSON.stringify({ ...stalled, before: undefined, after: undefined })}`)
    await step('typing: a key answered while the UI thread is busy past 2 s lands without an error', () => {
      assert.deepEqual(stalled.uncaught, [], 'errors reached the process')
      assert.deepEqual(stalled.missed, [], 'the key never painted')
    })
  }
  writeFileSync(path.join(out, 'typing.json'), JSON.stringify(report, null, 2))
}

let failed = false
try {
  // The lab first: on a cold runner the first launch (Bun's transpile cache, Mesa's
  // shader cache) can starve the app's daemon probe.
  if (only !== 'app') await shoot(path.join(import.meta.dir, '../native/clip-lab.tsx'), 'ready', `${os}-clip.png`, { KYBERN_DATA_DIR: data }, verifyLab)
  if (only !== 'lab') await shoot(app, 'app-ready', `${os}-app.png`, scratchEnv, verifyApp, `Show me the GPUIX client running on ${os}.`)
  if (only !== 'lab' && scrollScenario) await scrollBridge()
} catch (error) {
  failed = true
  console.error(`[smoke] FAILED: ${error instanceof Error ? error.message : error}`)
}
stop()
process.exit(failed ? 1 : 0)
