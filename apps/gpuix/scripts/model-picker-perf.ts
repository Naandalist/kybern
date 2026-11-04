/**
 * Typing and scrolling cost of the model picker over a large catalog (issue #218), on `scripts/model-picker-lab.tsx`.
 *
 *   bun scripts/model-picker-perf.ts [--out DIR] [--models 600] [--themes dark,light] [--rounds 3]
 *
 * Needs the patched addon (`native/build`) for KYBERN_FRAME_TIMES; scratch data under the temp folder's `gpuix-scratch`.
 *
 * Key to paint: each search key goes in through the automation channel (`keystrokes` on the search field) and is timed
 * from the moment it is sent until the first presented frame (`[kybern-frame]`, written by GPUI's `present`) that follows
 * the picker's React commit for that key (`[lab-commit]`, the lab's Profiler). Both lines come through the app's stderr
 * and are stamped on arrival here, so the boundary is: input sent → JS change → React commit → next present. It does not
 * include the compositor or the display. `commitMs` is React's own render time for that commit.
 * Scrolling: wheel steps every 16 ms over the list (down for half the time, then up) with 150 search results shown, and
 * the intervals between presented frames meanwhile. Screenshots of the open picker and of the results go to `--out`.
 */
import { mkdirSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { launchFixture } from '../src/requests/harness.ts'

const argv = process.argv.slice(2)
const arg = (name: string, fallback: string) => { const at = argv.indexOf(`--${name}`); return at >= 0 ? argv[at + 1]! : fallback }
const out = path.resolve(arg('out', path.join(tmpdir(), 'gpuix-scratch', 'model-picker-perf')))
const modelCount = Number(arg('models', '600'))
const themes = arg('themes', 'dark').split(',') as ('dark' | 'light')[]
const rounds = Number(arg('rounds', '3'))
const scratch = path.join(tmpdir(), 'gpuix-scratch', `model-picker-perf-${process.pid}`)
mkdirSync(out, { recursive: true })

const quantile = (values: number[], q: number) => {
  if (!values.length) return null
  const sorted = [...values].sort((a, b) => a - b)
  return +sorted[Math.min(sorted.length - 1, Math.floor(q * sorted.length))]!.toFixed(2)
}
const summary = (values: number[]) => ({ n: values.length, p50: quantile(values, 0.5), p90: quantile(values, 0.9), max: quantile(values, 1) })
const until = async (check: () => Promise<boolean> | boolean, label: string, ms = 10_000) => {
  const end = Date.now() + ms
  for (;;) { try { if (await check()) return } catch {} if (Date.now() > end) throw Error(`Timed out: ${label}`); await Bun.sleep(20) }
}

type Line = { at: number; kind: 'frame' | 'commit'; ms?: number }
const results: Record<string, unknown> = { models: modelCount, platform: process.platform, gpu: process.env.KYBERN_PERF_GPU ?? null }

for (const theme of themes) {
  const lines: Line[] = []
  let rest = ''
  const f = await launchFixture(path.join(import.meta.dir, 'model-picker-lab.tsx'), {
    GPUIX_BACKGROUND: '1', KYBERN_OPAQUE: '1', KYBERN_APPEARANCE: theme, KYBERN_FRAME_TIMES: '1', KYBERN_URL: '',
    KYBERN_DATA_DIR: path.join(scratch, 'data'), KYBERN_CLIENT_CONFIG_DIR: path.join(scratch, 'client'), MODEL_PICKER_LAB_MODELS: String(modelCount),
  }, {
    stderr: (chunk) => {
      const at = performance.now()
      const split = (rest + chunk).split('\n')
      rest = split.pop() ?? ''
      for (const line of split) {
        if (line.startsWith('[kybern-frame]')) lines.push({ at, kind: 'frame' })
        else if (line.startsWith('[lab-commit]')) lines.push({ at, kind: 'commit', ms: Number(line.split(' ')[2]) })
      }
    },
  })
  const app = f.app
  try {
    await until(async () => (await app.getByTestId('model-picker').count()) > 0, 'the trigger', 30_000)
    await app.getByTestId('model-picker').click()
    const search = app.getByTestId('model-search')
    await until(async () => (await search.count()) > 0, 'the picker opens')
    await Bun.sleep(800)
    await app.screenshot({ path: path.join(out, `open-${theme}.png`) })
    const field = await search.element()

    // One key: sent → the picker's next commit → the first frame presented after it.
    const keyToPaint = async (key: string) => {
      const sent = performance.now()
      const from = lines.length
      await app.call('keystrokes', { elementId: field.id, keys: key })
      let commit: Line | undefined, frame: Line | undefined
      await until(() => {
        for (let i = from; i < lines.length; i++) {
          const line = lines[i]!
          if (!commit && line.kind === 'commit') commit = line
          else if (commit && line.kind === 'frame') { frame = line; return true }
        }
        return false
      }, `key ${key} paints`, 3000)
      return { ms: frame!.at - sent, commitMs: commit!.ms ?? 0 }
    }
    const latencies: number[] = [], commitMs: number[] = []
    const typed = 'gemini flash 2'
    for (let round = 0; round < rounds; round++) {
      for (const char of typed) {
        const key = char === ' ' ? 'space' : char
        const r = await keyToPaint(key)
        latencies.push(r.ms); commitMs.push(r.commitMs)
        await Bun.sleep(120)
      }
      if (round === 0) await app.screenshot({ path: path.join(out, `typed-${theme}.png`) })
      // Back to the full list, untimed.
      await app.call('keystrokes', { elementId: field.id, keys: process.platform === 'darwin' ? 'cmd-a backspace' : 'ctrl-a backspace' })
      await Bun.sleep(400)
    }

    // Scrolling: 150 search results ("r" matches every lab model).
    await app.call('keystrokes', { elementId: field.id, keys: 'r' })
    await Bun.sleep(600)
    await app.screenshot({ path: path.join(out, `results-${theme}.png`) })
    const box = await search.bounds()
    const point = { x: box.x + box.width / 2, y: box.y + box.height + 140 }
    const scrollFrames: number[] = []
    for (let pass = 0; pass < rounds; pass++) {
      const from = lines.length
      const started = performance.now()
      let turned = false
      for (let step = 0; performance.now() - started < 2000; step++) {
        const down = performance.now() - started < 1000
        // The list as it stands at the turn: proof the wheel moved it.
        if (!down && !turned && pass === 0) { turned = true; await app.screenshot({ path: path.join(out, `scrolled-${theme}.png`) }) }
        await app.mouse.wheel(point, 0, down ? -40 : 40)
        await Bun.sleep(16)
      }
      const frames = lines.slice(from).filter((l) => l.kind === 'frame').map((l) => l.at)
      for (let i = 1; i < frames.length; i++) scrollFrames.push(frames[i]! - frames[i - 1]!)
    }
    const commitsWhileScrolling = lines.filter((l) => l.kind === 'commit').length
    results[theme] = {
      keyToPaintMs: summary(latencies),
      commitMs: summary(commitMs),
      scrollFrameIntervalMs: { ...summary(scrollFrames), over33: scrollFrames.filter((v) => v > 33.4).length, over50: scrollFrames.filter((v) => v > 50).length },
      totalCommits: commitsWhileScrolling,
    }
    console.log(theme, JSON.stringify(results[theme]))
  } catch (error) {
    await app.screenshot({ path: path.join(out, `failed-${theme}.png`) }).catch(() => {})
    console.log(f.diagnostics().slice(-2000))
    throw error
  } finally {
    await f.close()
  }
}
writeFileSync(path.join(out, 'model-picker-perf.json'), JSON.stringify(results, null, 2))
console.log(JSON.stringify(results, null, 2))
process.exit(0)
