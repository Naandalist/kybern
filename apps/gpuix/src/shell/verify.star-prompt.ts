/**
 * ADE-117 on screen: the "Enjoying Kybern?" toast (`KYBERN_STAR_PROMPT=force`), its layout in the theme, Star through `gh`
 * with the thanks toast, Star without `gh` opening the repository page, and Dismiss. Every close saves the dismissal.
 *
 *   KYBERN_DATA_DIR=<scratch daemon> KYBERN_STAR_GH=<dir> bun src/shell/verify.star-prompt.ts [--light]
 *
 * `<dir>/gh` is a fake `gh` that logs to `<dir>/gh.log` and answers per `<dir>/gh.mode` (`notstarred` | `unauth`). It
 * leads the app's PATH, so a daemon the app starts finds it; a scratch daemon already running must have it first on its
 * own PATH. Nothing reaches GitHub. `KYBERN_VERIFY_OUTPUT=<dir>` saves screenshots.
 */
import assert from 'node:assert/strict'
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import path from 'node:path'
import { launchFixture } from '../requests/harness.ts'
import { COPY, DISMISSED_KEY, REPOSITORY_URL } from './starPrompt.ts'

const mode = process.argv.includes('--light') ? 'light' : 'dark'
const ghDir = process.env.KYBERN_STAR_GH
if (!ghDir) throw new Error('Set KYBERN_STAR_GH to the fake gh directory')
const gh: string = ghDir
const out = process.env.KYBERN_VERIFY_OUTPUT
if (out) mkdirSync(out, { recursive: true })
const config = path.join(path.resolve(process.env.KYBERN_DATA_DIR!), `star-client-${mode}`)
const opened = path.join(config, 'opened.log')
const prefs = () => { try { return JSON.parse(readFileSync(path.join(config, 'app.json'), 'utf8')) as Record<string, unknown> } catch { return {} } }
const ghLog = () => (existsSync(path.join(gh, 'gh.log')) ? readFileSync(path.join(gh, 'gh.log'), 'utf8') : '')

/** Hover from outside the toast, so the window sees the pointer enter it even where a previous window left it. */
async function hoverToast(a: Awaited<ReturnType<typeof launchFixture>>['app']) {
  await Bun.sleep(500)
  const box = await a.getByTestId('toast-default').bounds()
  await a.mouse.move({ x: box.x - 200, y: box.y - 200 }); await Bun.sleep(100)
  await a.mouse.move({ x: box.x + box.width / 2, y: box.y + 20 })
  await a.getByTestId('toast-close').waitFor({ timeoutMs: 3000 })
}

async function session(ghMode: 'notstarred' | 'unauth', drive: (s: { a: Awaited<ReturnType<typeof launchFixture>>['app']; painted: () => Promise<string>; until: (re: RegExp) => Promise<void>; shot: (name: string) => Promise<void> }) => Promise<void>) {
  writeFileSync(path.join(gh, 'gh.mode'), ghMode)
  rmSync(path.join(gh, 'gh.log'), { force: true })
  rmSync(config, { recursive: true, force: true })
  mkdirSync(config, { recursive: true })
  const f = await launchFixture(path.join(import.meta.dir, '../main.tsx'), { KYBERN_WINDOW_SIZE: '1120x760', KYBERN_APPEARANCE: mode, KYBERN_STAR_PROMPT: 'force', KYBERN_CLIENT_CONFIG_DIR: config, KYBERN_OPEN_URL_LOG: opened, KYBERN_URL: '', PATH: `${gh}${path.delimiter}${process.env.PATH ?? ''}` })
  const a = f.app
  const painted = async () => (await a.call('getPaintedText', {})).text.join('\n')
  const until = async (re: RegExp) => { const end = Date.now() + 10000; while (Date.now() < end) { if (re.test(await painted())) return; await Bun.sleep(150) } assert.fail(`never painted ${re}: ${(await painted()).slice(-400)}`) }
  const shot = async (name: string) => { if (out) await a.screenshot({ path: path.join(out, `${name}-${mode}.png`) }) }
  try {
    await a.getByTestId('app-ready').waitFor({ timeoutMs: 60000 })
    await a.getByTestId('toast-default').waitFor({ timeoutMs: 10000 })
    await until(new RegExp(COPY.title))
    await drive({ a, painted, until, shot })
  } finally {
    await f.close()
  }
}

// Star with a signed-in gh: the PUT, the thanks, the dismissal.
await session('notstarred', async ({ a, painted, until, shot }) => {
  const text = await painted()
  for (const s of [COPY.title, COPY.body, COPY.dismiss, COPY.star]) assert.ok(text.includes(s), `the toast shows ${s}`)
  await Bun.sleep(700) // The entrance settles.
  await shot('star-toast')
  await hoverToast(a)
  assert.ok(await a.getByTestId('toast-close').count(), 'the X shows on hover')
  await Bun.sleep(300)
  await shot('star-toast-hover')
  // Stays put: no timer.
  await Bun.sleep(4500)
  assert.ok(await a.getByTestId('toast-default').count(), 'no timeout')
  await a.getByTestId('toast-action').click()
  await until(new RegExp(COPY.thanks))
  assert.match(ghLog(), /api -X PUT user\/starred\/Tresnanda\/kybern/)
  assert.equal(prefs()[DISMISSED_KEY], true, 'Star saves the dismissal')
  await Bun.sleep(500)
  await shot('star-thanks')
  assert.ok(!existsSync(opened), 'no browser when gh starred it')
})

// Star without a signed-in gh: the repository page opens.
await session('unauth', async ({ a }) => {
  await a.getByTestId('toast-action').click()
  const end = Date.now() + 8000
  while (Date.now() < end && !existsSync(opened)) await Bun.sleep(100)
  assert.equal(readFileSync(opened, 'utf8').trim(), `${REPOSITORY_URL}`)
  assert.equal(prefs()[DISMISSED_KEY], true)
})

// Dismiss, then the X: both save the dismissal.
await session('notstarred', async ({ a }) => {
  await a.getByTestId('toast-cancel').click(); await Bun.sleep(400)
  assert.equal(await a.getByTestId('toast-default').count(), 0, 'Dismiss closes it')
  assert.equal(prefs()[DISMISSED_KEY], true)
  assert.doesNotMatch(ghLog(), /PUT/)
})
await session('notstarred', async ({ a }) => {
  await hoverToast(a)
  await a.getByTestId('toast-close').click(); await Bun.sleep(400)
  assert.equal(await a.getByTestId('toast-default').count(), 0, 'the X closes it')
  assert.equal(prefs()[DISMISSED_KEY], true)
})
console.log(`[star-prompt] ${mode} ok`)
