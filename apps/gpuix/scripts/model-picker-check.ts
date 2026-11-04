/**
 * The composer's model picker on this platform (issue #218), on `scripts/model-picker-lab.tsx` (no daemon; a 600-model
 * OpenCode-style catalog):
 *
 *   bun scripts/model-picker-check.ts            (run-platform-checks: `--checks picker`)
 *
 * - focus: open the picker and click in its search field (the glyph, the padding left of it, the gap after it, the text,
 *   the far end), then send one key to whatever has native focus (no `elementId`, which would focus the input first):
 *   the key must filter the list, every time. Before the fix a reopened picker's search ignored keys (its input held a
 *   private focus handle), and a press on the field's box could leave it blurred. `PICKER_FOCUS_ROUNDS` (default 6)
 *   rounds of five clicks.
 * - keys: ↓ moves the highlight, Enter picks it and closes the picker, Escape closes it.
 * - list: a search shows 150 results in one native virtual list; the wheel scrolls it and later rows paint.
 * Screenshots (`KYBERN_VERIFY_OUTPUT`, else `native/build/picker`): the open picker, the results and the scrolled list,
 * dark and light.
 */
import assert from 'node:assert/strict'
import path from 'node:path'
import { readFileSync } from 'node:fs'
import { capture, captureSettled, launch, outputDir, settled, step, type Fixture } from '../native/check-harness.ts'

const out = outputDir('picker')
const rounds = Number(process.env.PICKER_FOCUS_ROUNDS ?? 6)
const lab = path.join(import.meta.dir, 'model-picker-lab.tsx')

type Node = { id: number; type: string; testId?: string; children?: Node[] }
/** Test ids of the listed model rows, from the element tree (React's output, whatever is painted). */
async function rowIds(f: Fixture): Promise<string[]> {
  const { tree } = await f.app.call('getTree', {}) as { tree: Node | null }
  const ids: string[] = []
  const walk = (node: Node) => { if (node.testId?.startsWith('model-row-')) ids.push(node.testId.slice('model-row-'.length)); node.children?.forEach(walk) }
  if (tree) walk(tree)
  return ids
}
async function find(f: Fixture, testId: string): Promise<Node | undefined> {
  const { tree } = await f.app.call('getTree', {}) as { tree: Node | null }
  const walk = (node: Node): Node | undefined => node.testId === testId ? node : node.children?.map(walk).find(Boolean)
  return tree ? walk(tree) : undefined
}
const sameFile = (a: string, b: string) => readFileSync(a).equals(readFileSync(b))
const qwenOnly = (ids: string[]) => ids.length > 0 && ids.every((id) => id.startsWith('qwen/'))

for (const theme of ['dark', 'light'] as const) {
  const f = await launch(lab, { KYBERN_APPEARANCE: theme, KYBERN_OPAQUE: '1', KYBERN_URL: '' })
  const app = f.app
  const search = app.getByTestId('model-search')
  const open = async () => {
    await settled(() => app.getByTestId('model-picker').count(), (n) => n > 0, 30_000)
    await app.getByTestId('model-picker').click()
    await settled(() => search.count(), (n) => n > 0, 5000)
    await settled(() => rowIds(f), (ids) => ids.some((id) => id.startsWith('anthropic/')), 5000)
  }
  const isOpen = async () => (await search.count()) > 0
  const close = async () => {
    if (!(await isOpen())) return
    await search.press('escape')
    await settled(isOpen, (open) => !open, 5000)
  }
  try {
    if (theme === 'dark') {
      await step('focus: a click anywhere in the search field leaves it focused', async () => {
        const failures: string[] = []
        const spots: [string, (b: { x: number; width: number }) => number][] = [
          ['glyph', (b) => b.x - 13], ['padding', (b) => b.x - 24], ['gap', (b) => b.x - 3], ['text', (b) => b.x + 4], ['end', (b) => b.x + b.width - 30],
        ]
        let clicks = 0
        for (let round = 0; round < rounds; round++) {
          for (const [spot, at] of spots) {
            await open()
            const box = await search.bounds()
            clicks++
            await app.mouse.click({ x: at(box), y: box.y + box.height / 2 })
            // "q" matches only the qwen backend's models.
            await app.call('keystrokes', { keys: 'q' })
            const ids = await settled(() => rowIds(f), qwenOnly, 3000)
            if (!qwenOnly(ids)) {
              failures.push(`round ${round} ${spot}${(await isOpen()) ? '' : ' (closed)'}`)
              await capture(f, path.join(out, `focus-failed-${round}-${spot}.png`)).catch(() => {})
            }
            await close()
          }
        }
        console.log(`focus: ${clicks} clicks, ${failures.length} lost${failures.length ? `: ${failures.join(', ')}` : ''}`)
        assert.deepEqual(failures, [], 'every click in the search field must leave it focused')
      })
      await step('keys: arrows move the highlight, Enter picks and closes, Escape closes', async () => {
        await open()
        // The search field has focus from autoFocus: no elementId, as a person types.
        await app.call('keystrokes', { keys: 'down down enter' })
        await settled(isOpen, (open) => !open, 5000)
        assert.equal(await isOpen(), false, 'Enter picks the highlighted row and closes the picker')
        await open()
        await app.call('keystrokes', { keys: 'escape' })
        await settled(isOpen, (open) => !open, 5000)
        assert.equal(await isOpen(), false, 'Escape closes the picker')
      })
    }
    await step(`list (${theme}): 150 results in one virtual list; the wheel scrolls it`, async () => {
      await open()
      await capture(f, path.join(out, `open-${theme}.png`))
      await app.call('keystrokes', { keys: 'r' })
      const ids = await settled(() => rowIds(f), (ids) => ids.length === 150, 5000)
      assert.equal(ids.length, 150, 'a search lists at most 150 rows')
      const list = await find(f, 'model-list')
      assert.equal(list?.type, 'virtual-list', 'a large catalog lists its rows in a native virtual list')
      const results = await capture(f, path.join(out, `results-${theme}.png`))
      // The list starts under the search field (the virtual list itself records no painted bounds).
      const field = await search.bounds()
      const point = { x: field.x + field.width / 2, y: field.y + field.height + 140 }
      for (let i = 0; i < 12; i++) await app.mouse.wheel(point, 0, -120)
      // The list moved: the window looks different with nothing else changed.
      const scrolled = await captureSettled(f, path.join(out, `scrolled-${theme}.png`), (file) => !sameFile(file, results))
      assert.ok(!sameFile(scrolled, results), 'the wheel scrolls the list')
      await close()
    })
  } finally {
    await f.close()
  }
}
console.log('PASS picker: focus after any click in the search field, arrows/Enter/Escape, the virtual list')
process.exit(0)
