/**
 * ADE-118: long unbreakable URLs and tokens wrap inside the user bubble (breaking anywhere when they must), the bubble is as
 * tall as its wrapped text, skill chips stay inside it, and assistant Markdown, inline code and the composer hold the same
 * input. `--light` for the light theme, `--narrow` for a 560 px window; `KYBERN_VERIFY_OUTPUT=<dir>` saves screenshots.
 */
import assert from 'node:assert/strict'
import { mkdirSync, mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { launchFixture } from '../requests/harness.ts'
import { TOKEN, URL_A } from './longlink.data.ts'

const mode = process.argv.includes('--light') ? 'light' : 'dark'
const narrow = process.argv.includes('--narrow')
const name = `${mode}${narrow ? '-narrow' : ''}`
const out = process.env.KYBERN_VERIFY_OUTPUT
if (out) mkdirSync(out, { recursive: true })
const scratch = mkdtempSync(path.join(tmpdir(), 'gpuix-longlink-'))
type Box = { x: number; y: number; width: number; height: number }
const f = await launchFixture(path.join(import.meta.dir, 'longlink.fixture.tsx'), { GPUIX_BACKGROUND: '1', KYBERN_APPEARANCE: mode, KYBERN_DATA_DIR: scratch, KYBERN_CLIENT_CONFIG_DIR: scratch, KYBERN_GPUIX_CREDENTIALS_DIR: scratch, KYBERN_GPUIX_ENVIRONMENT: 'local', KYBERN_URL: '', KYBERN_TOKEN: '', ...(narrow ? { KYBERN_LONGLINK_WIDTH: '560' } : {}) })
const a = f.app
/** Every element with this test id, with its box. */
const boxes = async (testId: string): Promise<Box[]> => Promise.all((await a.getByTestId(testId).all()).map(async (node: { id: number; bounds?: Box }) => node.bounds ?? (await a.call('getBounds', { elementId: node.id })).bounds as Box))
const inside = (inner: Box, outer: Box, slack = 1) => inner.x >= outer.x - slack && inner.x + inner.width <= outer.x + outer.width + slack
try {
  await a.getByTestId('fixture-ready').waitFor({ timeoutMs: 60000 })
  await a.getByTestId('token-text').waitFor({ timeoutMs: 10000 })
  await Bun.sleep(800)
  const pane = await a.getByTestId('fixture-ready').bounds()
  // Rows scrolled out of the virtual list have no box; the newest (the chips) is always painted.
  const bubbles = (await boxes('user-bubble')).filter((box): box is Box => !!box)
  assert.ok(bubbles.length >= 2, `${bubbles.length} bubbles painted`)
  for (const [i, box] of bubbles.entries()) {
    assert.ok(inside(box, pane), `bubble ${i} stays in the pane: ${JSON.stringify(box)} in ${pane.width}`)
    assert.ok(box.width <= pane.width * 0.8 + 1, `bubble ${i} keeps its 80% width: ${box.width}`)
  }
  // The chips bubble: every word and chip inside it, and it is no taller than its lines.
  const chips = await a.getByTestId('token-text').bounds()
  const chipBubble = bubbles.reduce((last, box) => (box.y > last.y ? box : last))
  assert.ok(inside(chips, chipBubble), `the token text stays inside its bubble: ${JSON.stringify(chips)} in ${JSON.stringify(chipBubble)}`)
  const skills = await boxes('inline-token-skill')
  assert.equal(skills.length, 5)
  for (const token of skills) assert.ok(inside(token, chipBubble) && token.y + token.height <= chipBubble.y + chipBubble.height, `each skill chip is inside the bubble: ${JSON.stringify(token)}`)
  // The bubble is its text, 12 px of padding, a 1 px edge each side and the 28 px hover footer: nothing empty under it.
  assert.ok(chipBubble.height - 28 - chips.height <= 16, `the bubble hugs its text: ${chipBubble.height} for ${chips.height} of text`)
  const text = (await a.call('getPaintedText', {})).text.join('')
  assert.ok(text.replace(/\s/g, '').includes(URL_A.replace(/\s/g, '').slice(-40)), 'the end of the long URL is painted')
  assert.ok(text.includes(TOKEN.slice(-12)), 'the end of the long token is painted')
  // The composer: the same URL typed in stays inside it.
  const editor = a.getByTestId('composer')
  await editor.fill(`Check ${URL_A} please`)
  await Bun.sleep(400)
  const composer = await editor.bounds()
  assert.ok(inside(composer, pane), 'the composer stays in the pane')
  if (out) await a.screenshot({ path: path.join(out, `longlink-${name}.png`) })
  console.log(`[longlink] ${name} ok`)
} catch (error) {
  if (out) await a.screenshot({ path: path.join(out, `longlink-${name}-failed.png`) }).catch(() => {})
  throw error
} finally {
  await f.close()
}
