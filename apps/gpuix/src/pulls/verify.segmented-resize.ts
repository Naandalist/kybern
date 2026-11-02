/**
 * ADE-112: a `fill` segmented control keeps its thumb on the selected segment while its container is dragged wider and
 * narrower. The Actions page's status filter (the run list's resize handle) and the dock Project panel's Coordinator
 * choices (the dock's resize edge), on the mock RPC fixture. `--light` for the light theme; `KYBERN_VERIFY_OUTPUT=<dir>`
 * saves before/after screenshots.
 */
import assert from 'node:assert/strict'
import { mkdirSync } from 'node:fs'
import { pixels } from '../../native/png.ts'
import path from 'node:path'
import { launchFixture } from '../requests/harness.ts'
import { actionFixture } from './verify.fixture.ts'
import { AUTONOMY_OPTIONS, WORKSPACE_OPTIONS } from '../../../../packages/kybern-client/src/coordinatorSettings.ts'

const mode = process.argv.includes('--light') ? 'light' : 'dark'
const out = process.env.KYBERN_VERIFY_OUTPUT
if (out) mkdirSync(out, { recursive: true })
const fixture = actionFixture({ coordinator: true })
const f = await launchFixture(path.join(import.meta.dir, '../main.tsx'), { KYBERN_WINDOW_SIZE: '1440x860', ...fixture.env, KYBERN_APPEARANCE: mode })
const a = f.app
type Box = { x: number; y: number; width: number; height: number }
const shot = async (name: string) => { if (out) await a.screenshot({ path: `${out}/${name}-${mode}.png` }) }
const click = async (id: string) => { await a.getByTestId(id).waitFor({ timeoutMs: 10000 }); await a.getByTestId(id).click(); await Bun.sleep(450) }
const bounds = (id: string): Promise<Box> => a.getByTestId(id).bounds()

const scratch = path.join(path.resolve(process.env.KYBERN_DATA_DIR!), 'segmented-resize')
mkdirSync(scratch, { recursive: true })
let frame = 0

/**
 * Where the thumb is painted along its track: the run of pixels 5 px below the track's top edge that differ from the
 * track's own fill (sampled in the 2 px inset above the thumb, at the middle), between the insets. The thumb slides by a paint-only
 * translation, which element bounds do not include, so the screenshot is the one source of truth for its x.
 */
async function paintedThumb(track: Box): Promise<{ x: number; width: number }> {
  const file = path.join(scratch, `frame-${frame++ % 4}.png`)
  await a.screenshot({ path: file })
  const image = pixels(file)
  const window = await bounds('app-ready')
  const scale = image.w / window.width
  const y = (track.y + 5) * scale
  const base = image.at((track.x + track.width / 2) * scale, (track.y + 1.25) * scale)
  const differs = (x: number) => image.at(x, y).reduce((sum, c, i) => sum + Math.abs(c - base[i]!), 0) > 18
  let left = -1, right = -1
  for (let x = Math.ceil((track.x + 2) * scale); x < (track.x + track.width - 2) * scale; x++) if (differs(x)) { if (left < 0) left = x; right = x }
  assert.ok(left >= 0, 'a thumb is painted on the track')
  return { x: left / scale, width: (right - left + 1) / scale }
}

/** The thumb covers the selected segment: its laid-out width (within a pixel) and its painted run (within its corners). */
async function matches(control: string, selected: string, label: string) {
  const end = Date.now() + 2000
  let last = ''
  while (Date.now() < end) {
    const [thumb, segment, track] = await Promise.all([bounds(`${control}-thumb`), bounds(`${control}-${selected}`), bounds(control)])
    const painted = await paintedThumb(track)
    last = `${label}: thumb ${thumb.width} painted ${JSON.stringify(painted)} segment ${JSON.stringify({ x: segment.x, width: segment.width })} track ${track.width}`
    // 3 px into the thumb its 8–9 px corner radius trims about 2.3 px at each end.
    if (Math.abs(thumb.width - segment.width) < 1 && Math.abs(painted.x - segment.x) < 3.5 && Math.abs(painted.x + painted.width - segment.x - segment.width) < 3.5) return track.width
    await Bun.sleep(60)
  }
  assert.fail(`the thumb is off its segment (${last})`)
}

/** The option whose segment the painted thumb covers. */
async function selectedOf(control: string, values: readonly string[]): Promise<string> {
  const painted = await paintedThumb(await bounds(control))
  for (const value of values) if (Math.abs((await bounds(`${control}-${value}`)).x - painted.x) < 3.5) return value
  assert.fail(`no ${control} segment under the thumb at ${painted.x}`)
}

/** Press on a resize edge and move the pointer by `dx` in steps, as a hand does; `during` runs before the release. */
async function drag(handle: string, dx: number, during?: () => Promise<void>) {
  const box = await bounds(handle)
  const start = { x: box.x + box.width / 2, y: box.y + box.height / 2 }
  await a.mouse.move({ x: start.x, y: start.y - 40 }); await Bun.sleep(80)
  await a.mouse.move(start); await Bun.sleep(80)
  await a.mouse.down(start); await Bun.sleep(80)
  const steps = 8
  for (let i = 1; i <= steps; i++) { await a.mouse.move({ x: start.x + (dx * i) / steps, y: start.y }, { pressedButton: 0 }); await Bun.sleep(30) }
  if (during) await during()
  await a.mouse.up({ x: start.x + dx, y: start.y })
  await Bun.sleep(300)
}

try {
  await a.getByTestId('app-ready').waitFor({ timeoutMs: 60000 })

  // The Actions page: Failed selected (the third segment, so the thumb has moved off the first slot).
  await click('rail-pulls')
  await click('pulls-tab-actions')
  await a.getByTestId('actions-status').waitFor({ timeoutMs: 10000 })
  await click('actions-status-failure')
  const narrow = await matches('actions-status', 'failure', 'actions at rest')
  await shot('actions-before')
  await drag('actions-list-resize', 200, async () => { await matches('actions-status', 'failure', 'actions while dragging') })
  const wide = await matches('actions-status', 'failure', 'actions wider')
  assert.ok(wide > narrow + 100, `the list grew (${narrow} → ${wide})`)
  await shot('actions-wide')
  await click('actions-status-success')
  await matches('actions-status', 'success', 'actions wider, another segment')
  await drag('actions-list-resize', -260)
  const back = await matches('actions-status', 'success', 'actions narrower')
  assert.ok(back < wide - 100, `the list shrank (${wide} → ${back})`)
  await shot('actions-narrow')

  // The dock's Project panel on the Coordinator thread.
  await click('rail-home')
  await click('thread-linked')
  await click('dock-toggle')
  await click('dock-add')
  await click('dock-add-project')
  await a.getByTestId('dock-project-autonomy').waitFor({ timeoutMs: 10000 })
  // The fixture keeps the settings' defaults (it refuses `projects.update`): check the segments those select.
  const selectedAutonomy = await selectedOf('dock-project-autonomy', AUTONOMY_OPTIONS.map((o) => o.value))
  const workspace = await selectedOf('dock-project-workspace', WORKSPACE_OPTIONS.map((o) => o.value))
  const dockNarrow = await matches('dock-project-workspace', workspace, 'dock at rest')
  await matches('dock-project-autonomy', selectedAutonomy, 'dock autonomy at rest')
  await shot('dock-before')
  await drag('dock-resize', -220, async () => { await matches('dock-project-workspace', workspace, 'dock while dragging') })
  const dockWide = await matches('dock-project-workspace', workspace, 'dock wider')
  await matches('dock-project-autonomy', selectedAutonomy, 'dock autonomy wider')
  assert.ok(dockWide > dockNarrow + 100, `the dock grew (${dockNarrow} → ${dockWide})`)
  await shot('dock-wide')
  await drag('dock-resize', 260)
  const dockBack = await matches('dock-project-workspace', workspace, 'dock narrower')
  await matches('dock-project-autonomy', selectedAutonomy, 'dock autonomy narrower')
  assert.ok(dockBack < dockWide - 100, `the dock shrank (${dockWide} → ${dockBack})`)
  console.log(`[segmented-resize] ${mode} ok`)
} finally {
  await f.close()
  fixture.close()
}
