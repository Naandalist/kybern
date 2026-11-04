/**
 * The pull request page's problem states, mock RPC only: gh not installed and gh signed out (the daemon's `data.problem`),
 * an older daemon's signed-out text, and a plain failure with its Details disclosure. "Check again" re-asks the daemon,
 * and the raw RPC text is never the headline. `bun src/pulls/verify.problem.ts [--light]`
 */
import assert from 'node:assert/strict'
import { mkdirSync } from 'node:fs'
import path from 'node:path'
import { launchFixture } from '../requests/harness.ts'
import { captureWindow } from '../transcript/live.ts'
import { actionFixture } from './verify.fixture.ts'

const mode = process.argv.includes('--light') ? 'light' : 'dark'
const out = process.env.KYBERN_VERIFY_OUTPUT ?? path.join(import.meta.dir, '../../../../.kybern-artifacts/gh-problem')
mkdirSync(out, { recursive: true })
const fixture = actionFixture({ problem: 'missing' })
const f = await launchFixture(path.join(import.meta.dir, '../main.tsx'), { ...fixture.env, KYBERN_APPEARANCE: mode })
const a = f.app
const shot = async (name: string) => { const file = path.join(out, `${name}-${mode}.png`); try { captureWindow(f.pid, file) } catch { await a.screenshot({ path: file }) }; console.log(`[problem] ${file}`) }
const click = async (id: string) => { await a.getByTestId(id).waitFor({ timeoutMs: 10000 }); await a.getByTestId(id).click(); await Bun.sleep(500) }
const painted = async () => (await a.call('getPaintedText', {})).text.join('\n')
const lists = () => fixture.calls.filter(m => m === 'github.pr.list').length
try {
  await a.getByTestId('app-ready').waitFor({ timeoutMs: 60000 })
  await click('rail-pulls')
  await a.getByTestId('pr-problem').waitFor({ timeoutMs: 10000 })
  await Bun.sleep(400)
  let text = await painted()
  assert.match(text, /Install GitHub CLI to see pull requests/)
  assert.match(text, /your own GitHub sign-in/)
  assert.match(text, /Check again/)
  assert.doesNotMatch(text, /RpcCallError|program not found/, 'the raw error stays behind Details')
  await shot('1-gh-missing')
  // Install opens a terminal sheet only for this computer's own daemon; this fixture is a remote endpoint, so it isn't clicked here.

  fixture.problem('auth')
  const before = lists()
  await click('pr-problem-secondary')
  await Bun.sleep(500)
  assert.ok(lists() > before, 'Check again asks the daemon again')
  text = await painted()
  assert.match(text, /Sign in to GitHub to see pull requests/)
  assert.match(text, /Sign in to GitHub/)
  await shot('2-gh-signed-out')

  // An older daemon sends only text: the same state, sorted from it.
  fixture.problem('auth-text')
  await click('pr-problem-secondary')
  assert.match(await painted(), /Sign in to GitHub to see pull requests/)

  fixture.problem('other')
  await click('pr-problem-secondary')
  text = await painted()
  assert.match(text, /Unable to load pull requests/)
  assert.doesNotMatch(text, /no such host/, 'detail hidden until asked')
  await shot('3-generic')
  await click('pr-problem-details')
  text = await painted()
  assert.match(text, /no such host/)
  assert.match(text, /Copy details/)
  await shot('4-generic-details')

  fixture.problem(null)
  await click('pr-problem-secondary')
  await a.getByTestId('pr-list').waitFor({ timeoutMs: 10000 })
  assert.equal(await a.getByTestId('pr-problem').count(), 0, 'the list comes back once gh works')
  console.log(`[problem] ok (${mode})`)
} finally {
  await a.close().catch(() => {})
  fixture.close()
}
