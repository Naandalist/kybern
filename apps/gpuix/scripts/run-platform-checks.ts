/**
 * Run the cross-platform on-screen checks (WP7) one after another, each in its own Bun process, and summarize.
 *
 *   bun scripts/run-platform-checks.ts --kybernd <path to kybernd> [--out gpuix-checks] [--checks all|clip,rounded,...]
 *                                      [--strict] [--capture none] [--list]
 *
 * Linux needs a display: `xvfb-run -a -s '-screen 0 1440x900x24 -extension Composite' bun scripts/run-platform-checks.ts ...`.
 * Windows and macOS use the desktop session. The checks are the same files a developer runs by hand on a Mac
 * (`native/*-check.ts`, `scripts/platform-smoke.ts`, `scripts/nowebview-app-check.ts`); this runner only gives each a scratch
 * data directory, an output folder, a time limit and a shared event report (`report.jsonl`, written by `step` in
 * `native/check-harness.ts`), then writes `summary.md` / `summary.json` next to the evidence. The exit code is 1 when a check
 * fails or times out. `--strict` (or KYBERN_CHECK_STRICT=1) makes the known gaps listed in the harness failures.
 *
 * `--capture none` is for a display nothing can grab (headless Weston): the smoke check runs once without PNGs, asserting that
 * the windows come up and the seeded message is painted.
 *
 * Output layout (`--out`): `<check>/` holds that check's PNGs and `log.txt`; `report.jsonl` and `summary.*` sit at the top.
 * Scratch data (daemon sockets, databases) lives under the temp folder in a `gpuix-scratch` directory, never in `--out`, so
 * the whole `--out` folder can be uploaded as an artifact.
 */
import { spawn } from 'node:child_process'
import { appendFileSync, copyFileSync, existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

const root = path.resolve(import.meta.dir, '..')
const argv = process.argv.slice(2)
const flag = (name: string) => argv.includes(`--${name}`)
const value = (name: string, fallback?: string) => {
  const at = argv.indexOf(`--${name}`)
  const v = at >= 0 ? argv[at + 1] : fallback
  if (v === undefined) throw new Error(`--${name} is required`)
  return v
}
const platform = process.platform === 'darwin' ? 'macos' : process.platform === 'win32' ? 'windows' : 'linux'

interface Run { name: string; args: string[]; env?: Record<string, string>; minutes?: number }
interface Check { description: string; runs: (ctx: { kybernd: string; out: string; scratch: string; capture: string }) => Run[] }

const native = (file: string) => path.join(root, 'native', file)
const script = (file: string) => path.join(root, 'scripts', file)
// What frame-wake-check must find: GPUI's platform name (WP1). macOS is not asserted.
const display = platform === 'windows' ? 'windows' : platform === 'linux' ? (process.env.WAYLAND_DISPLAY && !process.env.DISPLAY ? 'Wayland' : 'X11') : undefined

/** What the Linux/Windows job runs; `all` is every key, in this order. */
const CHECKS: Record<string, Check> = {
  focus: {
    description: 'Tab and Shift+Tab move focus one stop; a consumed Tab stays; the image preview traps Tab (no pixels)',
    runs: () => [{ name: 'focus', args: [native('focus-check.ts')], minutes: 10 }],
  },
  picker: {
    description: 'The model picker (#218) on a 600-model catalog: 30 clicks anywhere in its search field each leave it focused (a key filters the list), arrows/Enter/Escape, 150 results in one native virtual list that the wheel scrolls',
    runs: ({ capture }) => capture === 'none' ? [] : [{ name: 'picker', args: [script('model-picker-check.ts')], minutes: 10 }],
  },
  clip: {
    description: 'Rounded overflow clip: hit tests, painted corners, cached replay, virtual list, code scroll/wrap, dark/light x motion',
    runs: () => [{ name: 'clip', args: [native('clip-check.ts')], minutes: 25 }],
  },
  rounded: {
    description: 'Rounded input backgrounds: painted corners, outset clamps, wrapped fragments, selection overlay',
    runs: () => [{ name: 'rounded', args: [native('rounded-check.ts')], minutes: 15 }],
  },
  backdrop: {
    description: 'WP9: the backdrop filter on this renderer (blur, tint, radius and rounded-chain clip, saturation, sharp content; the filter-off fallback off macOS) and its frame cost over a busy scene',
    runs: () => [{ name: 'backdrop', args: [native('backdrop-check.ts')], minutes: 10 }],
  },
  'markdown-document': {
    description: 'Markdown document API and parity: document vs original pixels, selection, wrap, grid; a subset of the interior cases off macOS',
    runs: () => [{
      name: 'markdown-document',
      args: [native('markdown-document-check.ts')],
      // The macOS run covers all twelve interior cases (~100 launches); software rendering gets the structural ones. Override with MD_INTERIOR_CASES.
      env: { MD_INTERIOR_CASES: process.env.MD_INTERIOR_CASES ?? (platform === 'macos' ? '' : 'semantics,table,code,wrappedcode') },
      minutes: 40,
    }],
  },
  text: {
    description: 'WP8: kit weights (450/550/650) on the wght axis, line boxes and baselines vs macOS, the code face advance, kszl inline code, emoji and CJK',
    runs: () => [{ name: 'text', args: [native('text-check.ts')], minutes: 5 }],
  },
  smoke: {
    description: 'The app on a seeded daemon (sidebar, Markdown + code transcript, composer) and the clip lab, rounded clip on and off',
    // Without captures the clip switch cannot be told apart, so one run is enough.
    runs: ({ kybernd, out, scratch, capture }) => (capture === 'none' ? [1] : [1, 0]).map(clip => ({
      name: `smoke-clip-${clip}`,
      args: [
        script('platform-smoke.ts'), '--kybernd', kybernd, '--out', path.join(out, `smoke-clip-${clip}`), '--data', path.join(scratch, `smoke-clip-${clip}`),
        '--port', String(4377 + clip), ...(capture === 'none' ? ['--capture', 'none'] : []),
        // The transcript-scroll bridge scenario (WP1) runs once, with the clip on.
        ...(clip ? [] : ['--scroll', '0']),
      ],
      env: { KYBERN_ROUNDED_CLIP: String(clip) },
      minutes: 20,
    })),
  },
  nowebview: {
    description: 'WP4: the real app with the web view forced off (preview, visual, question, Mermaid, artifact) on a seeded daemon',
    runs: ({ kybernd, scratch }) => [{
      name: 'nowebview',
      args: [script('nowebview-app-check.ts'), 'dark'],
      env: { KYBERND: kybernd, VISUAL_SCRATCH: path.join(scratch, 'nowebview') },
      minutes: 20,
    }],
  },
  webview: {
    description: 'WP10: the embedded web view in a GPUIX window. Windows: WebView2 (stacking, rounded clip, menu cut-out, dim, input, bridge, draft, present, snapshots). Linux: WPE drawn by <websurface> (the lab: bridge, forged child post, rounded clip, transparency, popup over the page, click draft, <select>, typing, snapshot, hide; pixels only with captures; needs native/build/libkybern-webview.so and native/build/wpe)',
    runs: ({ out, capture }) => platform === 'windows'
      ? [{ name: 'webview', args: [native('webview-win-check.ts'), '--out', path.join(out, 'webview')], minutes: 10 }]
      : platform === 'linux' ? [{ name: 'webview', args: [native('webview-linux-check.ts')], env: capture === 'none' ? { KYBERN_CHECK_CAPTURE: 'none' } : {} as Record<string, string>, minutes: 10 }] : [],
  },
  'webview-app': {
    description: 'WP10 (Linux): the real app with the embedded web view on a seeded daemon (live visual, a draft from a click in the page, Mermaid drawn, the preview in its embedded slot, no browser opened)',
    runs: ({ kybernd, scratch, capture }) => platform === 'linux' && capture !== 'none' ? [{
      name: 'webview-app',
      args: [script('nowebview-app-check.ts'), 'dark', 'embedded'],
      env: { KYBERND: kybernd, VISUAL_SCRATCH: path.join(scratch, 'webview-app') },
      minutes: 20,
    }] : [],
  },
  'webview-sandbox': {
    description: 'WP10 (Linux): with a Snap environment or WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS the web view falls back to the browser and no web process starts (JS probe, companion), WebKit itself aborts rather than run one unsandboxed and ignores the variable (needs native/build/libkybern-webview.so and native/build/wpe)',
    runs: () => platform === 'linux' ? [{ name: 'webview-sandbox', args: [native('webview-linux-sandbox-check.ts')], minutes: 5 }] : [],
  },
  notify: {
    description: 'WP5: system notifications through the addon. Linux: a test D-Bus notification server (Notify fields, replace per thread, ActionInvoked reaches the app once, no daemon = unsupported). Windows: the toast API with the AUMID, its registry entry, the posts and the history',
    runs: () => platform === 'macos' ? [] : [{ name: 'notify', args: [native('notify-check.ts')], minutes: 10 }],
  },
  'window-chrome': {
    description: 'WP3: the title bar per OS (window buttons, Windows caption hit tests with real input, Linux client-side decorations under a compositing window manager), drag, double click, minimize/maximize/close, resize edges, the Ctrl shortcut key',
    runs: ({ kybernd, capture }) => capture === 'none' ? [] : [{ name: 'window-chrome', args: [script('window-chrome-check.ts'), '--kybernd', kybernd], minutes: 10 }],
  },
  'frame-wake': {
    description: 'WP1: no JS polling or per-frame wakes, onFrame delivery, unwaited bridge calls, scroll phases, repaint of cached islands at both parities (and a KYBERN_FULL_INVALIDATE=1 control), the UI-exit wake, DPI info (Windows), the Wayland seat',
    // KYBERN_EXPECT_SEAT: what getPlatformInfo().seat must be (the headless Weston job sets false).
    runs: ({ out, capture }) => [false, true].filter(full => !full || (platform !== 'macos' && capture !== 'none')).map(full => ({
      name: full ? 'frame-wake-full' : 'frame-wake',
      args: [
        script('frame-wake-check.tsx'), '--out', path.join(out, full ? 'frame-wake-full' : 'frame-wake', `frame-wake-${(display ?? platform).toLowerCase()}.json`),
        ...(display ? ['--expect-platform', display] : []), ...(process.env.KYBERN_EXPECT_SEAT ? ['--expect-seat', process.env.KYBERN_EXPECT_SEAT] : []),
        ...(capture === 'none' ? ['--capture', 'none'] : []),
      ],
      ...(full ? { env: { KYBERN_FULL_INVALIDATE: '1' } } : {}),
      minutes: 5,
    })),
  },
  // The composer's selection-dependent behaviour (#215) through its own verify scripts, on the conversation fixture (no
  // daemon); each writes its PNGs to KYBERN_VERIFY_OUTPUT. Not under a display nothing can grab.
  ...Object.fromEntries(([
    ['editor', 'Composer: a picked mention is one atomic token (one Backspace removes it), undo/redo walk the edits, Enter sends structured parts'],
    ['skill-pick', 'Composer: a skill picked from the $ menu (Enter or a click, at the end or before text) becomes its chip with the caret after it'],
    ['skill-token', 'Composer: a picked skill paints as its accent chip on the first frame, before any further typing'],
    ['conversation', 'Composer: note, task and Computer mentions, a completion in the middle of text, the Coordinator draft and harness switch'],
  ] as const).map(([name, description]): [string, Check] => [name, {
    description,
    runs: ({ capture }) => capture === 'none' ? [] : [{ name, args: [path.join(root, 'src', 'composer', `${name}.verify.ts`)], minutes: 10 }],
  }])),
}

if (flag('list')) {
  for (const [name, check] of Object.entries(CHECKS)) console.log(`${name.padEnd(18)} ${check.description}`)
  process.exit(0)
}

const kybernd = path.resolve(value('kybernd', process.env.KYBERND))
const out = path.resolve(value('out', 'gpuix-checks'))
const capture = value('capture', 'png')
const selected = value('checks', 'all').split(',').map(s => s.trim()).filter(Boolean)
const names = selected.includes('all') ? Object.keys(CHECKS) : selected
for (const name of names) if (!CHECKS[name]) throw new Error(`Unknown check "${name}". Known: ${Object.keys(CHECKS).join(', ')}`)
// The scratch path must contain `gpuix-scratch`: the app fixtures refuse a data directory without it.
const scratch = path.join(tmpdir(), 'gpuix-scratch', `wp7-${process.pid}`)
rmSync(out, { recursive: true, force: true })
mkdirSync(out, { recursive: true })
mkdirSync(scratch, { recursive: true })
const reportFile = path.join(out, 'report.jsonl')
writeFileSync(reportFile, '')
const strict = flag('strict') || process.env.KYBERN_CHECK_STRICT === '1'

interface Result { run: string; check: string; status: 'pass' | 'fail' | 'timeout'; seconds: number; images: number; tail: string[] }
const results: Result[] = []

const pngs = (dir: string): number => {
  if (!existsSync(dir)) return 0
  return readdirSync(dir, { withFileTypes: true }).reduce((n, e) => n + (e.isDirectory() ? pngs(path.join(dir, e.name)) : e.name.endsWith('.png') ? 1 : 0), 0)
}

async function execute(checkName: string, run: Run) {
  const dir = path.join(out, run.name)
  mkdirSync(dir, { recursive: true })
  const dataDir = path.join(scratch, run.name)
  mkdirSync(dataDir, { recursive: true })
  const log = path.join(dir, 'log.txt')
  writeFileSync(log, '')
  console.log(`\n=== ${run.name} (${platform}) ===`)
  const started = Date.now()
  const child = spawn(process.execPath, run.args, {
    env: {
      ...process.env,
      KYBERN_DATA_DIR: dataDir,
      KYBERN_VERIFY_OUTPUT: dir,
      KYBERN_CHECK_NAME: run.name,
      KYBERN_CHECK_REPORT: reportFile,
      KYBERN_CHECK_STRICT: strict ? '1' : '0',
      // A KYBERN_URL inherited from the calling shell may be a real daemon; launchFixture refuses it.
      KYBERN_URL: '',
      KYBERN_TOKEN: '',
      ...run.env,
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  const tail: string[] = []
  const sink = (chunk: Buffer) => {
    const text = chunk.toString()
    process.stdout.write(text)
    appendFileSync(log, text)
    // A diagnostics dump is one huge line; the summary keeps its start (log.txt has all of it).
    for (const line of text.split('\n')) if (line.trim()) { tail.push(line.length > 400 ? `${line.slice(0, 400)}…` : line); if (tail.length > 25) tail.shift() }
  }
  child.stdout.on('data', sink)
  child.stderr.on('data', sink)
  let timedOut = false
  const timer = setTimeout(() => { timedOut = true; child.kill() }, (run.minutes ?? 20) * 60_000)
  const code = await new Promise<number | null>(resolve => child.once('exit', resolve))
  clearTimeout(timer)
  const daemonLog = path.join(dataDir, 'daemon.log')
  if (existsSync(daemonLog)) copyFileSync(daemonLog, path.join(dir, 'daemon.log'))
  results.push({
    run: run.name,
    check: checkName,
    status: timedOut ? 'timeout' : code === 0 ? 'pass' : 'fail',
    seconds: Math.round((Date.now() - started) / 1000),
    images: pngs(dir),
    tail: timedOut || code !== 0 ? tail.slice(-15) : [],
  })
  console.log(`=== ${run.name}: ${results.at(-1)!.status} in ${results.at(-1)!.seconds} s, ${results.at(-1)!.images} images ===`)
}

for (const name of names) for (const run of CHECKS[name]!.runs({ kybernd, out, scratch, capture })) await execute(name, run)

// ---- summary ----
const events = existsSync(reportFile) ? readFileSync(reportFile, 'utf8').split('\n').filter(Boolean).map(l => JSON.parse(l) as { check: string; step: string; status: string; detail?: string }) : []
const gaps = events.filter(e => e.status === 'gap')
const closed = events.filter(e => e.status === 'gap-closed')
const lines = [
  `## GPUIX on-screen checks: ${platform}${strict ? ' (strict)' : ''}`,
  '',
  '| Run | Result | Time | Images |',
  '| --- | --- | --- | --- |',
  ...results.map(r => `| ${r.run} | ${r.status === 'pass' ? 'pass' : `**${r.status}**`} | ${r.seconds} s | ${r.images} |`),
  '',
]
if (gaps.length) lines.push('### Known gaps hit (not failures)', '', ...gaps.map(g => `- \`${g.check}\` · ${g.step}: ${g.detail}`), '')
if (closed.length) lines.push('### Gaps that now pass (remove the `gap` marker)', '', ...closed.map(g => `- \`${g.check}\` · ${g.step}: ${g.detail}`), '')
for (const r of results.filter(r => r.status !== 'pass')) lines.push(`### ${r.run} ${r.status}`, '', '```', ...r.tail, '```', '')
writeFileSync(path.join(out, 'summary.md'), lines.join('\n'))
writeFileSync(path.join(out, 'summary.json'), JSON.stringify({ platform, strict, results, gaps, closed }, null, 2))
if (process.env.GITHUB_STEP_SUMMARY) appendFileSync(process.env.GITHUB_STEP_SUMMARY, lines.join('\n') + '\n')
console.log('\n' + lines.join('\n'))
// A daemon that has not exited yet can hold files open on Windows; the runner's temp folder is thrown away anyway.
try { rmSync(scratch, { recursive: true, force: true }) } catch {}
process.exit(results.every(r => r.status === 'pass') ? 0 : 1)
