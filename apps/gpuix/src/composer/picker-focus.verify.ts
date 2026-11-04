// Issue #218: a click anywhere in the model picker's search field must leave it focused, so the next key filters the list.
// Off macOS the input's blur used to reach JS after the shell's click, and `TextField` skipped the refocus on a stale flag.
// Real app, scratch daemon; keys go to whatever has native focus (no `elementId`, which would focus the input first).
//   PICKER_FOCUS_ROUNDS (default 10) rounds of: open the picker, click the glyph, the padding, the text, the gap after it;
//   each click is followed by a key that must narrow the list.
import assert from 'node:assert/strict'
import { mkdirSync } from 'node:fs'
import { EVIDENCE, launchLive, until } from '../transcript/live.ts'
import { connect, readState } from '../../visual/fixture.ts'

const out = process.env.KYBERN_VERIFY_OUTPUT ?? `${EVIDENCE}/picker-focus`
mkdirSync(out, { recursive: true })
const rounds = Number(process.env.PICKER_FOCUS_ROUNDS ?? 10)
const themes = (process.env.PICKER_FOCUS_THEMES ?? 'dark').split(',') as ('dark' | 'light')[]

for (const theme of themes) {
  const title = `Picker focus ${theme} ${Date.now()}`
  const seed = await connect(readState()!)
  const { projects } = await seed.call('projects.list', {})
  await seed.call('threads.create', { project_id: projects[0]!.id, provider: { kind: 'claude-code', instance: 'default' }, title } as never)
  seed.close?.()
  const f = await launchLive({ theme, thread: title })
  const failures: string[] = []
  let clicks = 0
  try {
    const app = f.app
    const text = async () => { await app.clock.fastForward(16); return String((await app.call('getAllText', {})).text) }
    const search = app.getByTestId('model-search')
    const open = async () => {
      await until(async () => (await app.getByTestId('model-picker').count()) > 0, 'picker trigger', 20000)
      await app.getByTestId('model-picker').click()
      await until(async () => (await search.count()) > 0, 'picker opens')
      await until(async () => (await text()).includes('Sonnet'), 'the list paints')
    }
    const closed = () => until(async () => (await search.count()) === 0, 'picker closes')
    // The field: 8 px row padding, then the shell's 10 px padding, the 14 px glyph, a 6 px gap and the input.
    const spots: [string, (input: { x: number; width: number }) => number][] = [
      ['glyph', (i) => i.x - 13], ['padding', (i) => i.x - 24], ['gap', (i) => i.x - 3], ['text', (i) => i.x + 4], ['end', (i) => i.x + i.width - 30],
    ]
    for (let round = 0; round < rounds; round++) {
      for (const [spot, at] of spots) {
        // As in the issue: open the picker (its search is autoFocus), click in the field, type.
        await open()
        const input = await search.bounds()
        clicks++
        await app.mouse.click({ x: at(input), y: input.y + input.height / 2 })
        // One key to whatever has native focus.
        await app.call('keystrokes' as never, { keys: 'h' } as never)
        try {
          await until(async () => { const t = await text(); return t.includes('Haiku 4.5') && !t.includes('Sonnet') }, `${spot}: the key filters the list`, 3000)
        } catch {
          const stillOpen = (await search.count()) > 0
          failures.push(`round ${round} ${spot}${stillOpen ? '' : ' (picker closed)'}`)
          await app.screenshot({ path: `${out}/failed-${theme}-${round}-${spot}.png` }).catch(() => {})
        }
        // Close through the input (this focuses it first); a picker whose search ignores keys closes from its trigger.
        if ((await search.count()) > 0) {
          await search.press('escape')
          await closed().catch(async () => { await app.getByTestId('model-picker').click(); await closed() })
        }
      }
    }
    await open()
    await app.screenshot({ path: `${out}/open-${theme}.png` })
    await search.press('escape')
    await closed().catch(async () => { await app.getByTestId('model-picker').click(); await closed() })
    console.log(`${theme}: ${clicks} clicks, ${failures.length} lost focus${failures.length ? `: ${failures.join(', ')}` : ''}`)
    assert.deepEqual(failures, [], 'every click in the search field must leave it focused')
  } catch (error) { await f.app.screenshot({ path: `${out}/failed-${theme}.png` }).catch(() => {}); console.log(f.diagnostics().slice(-1500)); throw error } finally {
    const id = f.thread?.id
    await f.close()
    if (id) { const c = await connect(readState()!); await c.call('threads.archive', { thread_id: id }).catch(() => {}); c.close?.() }
  }
}
process.exit(0)
