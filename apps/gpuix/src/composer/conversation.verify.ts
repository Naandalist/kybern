import assert from 'node:assert/strict'
import { mkdtempSync, rmSync, mkdirSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { launchFixture } from '../requests/harness.ts'
async function until(check: () => Promise<boolean>) { for(let i=0;i<100;i++) { try { if(await check()) return } catch {}; await Bun.sleep(50) } throw Error('Condition timed out') }
const out = process.env.KYBERN_VERIFY_OUTPUT ?? '/Users/mymac/projects/ade/previews/gpuix/p3'
mkdirSync(out, { recursive: true })
for (const variant of ['dark', 'light']) {
  const scratch = mkdtempSync(tmpdir() + '/gpuix-p3-conversation-')
  const f = await launchFixture(import.meta.dir + '/conversation.fixture.tsx', { GPUIX_BACKGROUND: '1', KYBERN_APPEARANCE: variant, KYBERN_DATA_DIR: scratch, KYBERN_CLIENT_CONFIG_DIR: scratch, KYBERN_GPUIX_CREDENTIALS_DIR: scratch, KYBERN_GPUIX_ENVIRONMENT: 'local', KYBERN_URL: '', KYBERN_TOKEN: '' })
  try {
    const app = f.app
    await app.getByTestId('conversation-ready').waitFor()
    console.log('CHECK connected', variant)
    await until(async () => await app.getByTestId('connected').textContent() === 'open')
    await until(async () => Number(await app.getByTestId('overlay-height').textContent()) > 80)
    const initialHeight = Number(await app.getByTestId('overlay-height').textContent())
    // Picked tokens are inline atomic labels in the editor itself; read them from the painted composer text.
    const painted = async () => String((await app.call('getPaintedText', {})).text)
    const shown = async (label: string) => (await painted()).includes(label)
    for (const [query, path, label] of [['Release','kybern://note/note','Release notes'], ['KYB-1','kybern://task/task','KYB-1 Release checklist'], ['Computer','kybern://computer','Computer']] as const) {
      console.log('CHECK mention', query)
      await app.getByTestId('composer').fill('@' + query)
      await until(async () => (await app.getByTestId('completion-selected').textContent()).includes(query!))
      await app.getByTestId('composer').press('enter')
      await until(async () => (await app.getByTestId('completion').count()) === 0)
      await until(() => shown(label))
      await app.screenshot({ path: `${out}/tokens-${variant}.png` })
      if (query !== 'Computer') {
      // Undo returns the raw `@query`, which reopens the picker; redo restores the atom.
      await app.getByTestId('composer').press('cmd-z')
      await until(async () => (await app.getByTestId('completion').count()) > 0)
      await app.getByTestId('composer').press('cmd-shift-z')
      await until(async () => (await app.getByTestId('completion').count()) === 0)
      }
      await until(() => shown(label))
      await app.getByTestId('composer').press('enter')
      await until(async () => (await app.getByTestId('message-record').textContent()).includes(path!))
      assert.ok(JSON.parse(await app.getByTestId('message-record').textContent()).parts.some((part: { path?: string }) => part.path === path))
    }
    console.log('CHECK middle completion')
    await app.getByTestId('composer').fill('Before @Release after')
    for (let i = 0; i < 6; i++) await app.getByTestId('composer').press('left')
    await until(async () => (await app.getByTestId('completion-selected').textContent()).includes('Release'))
    await app.getByTestId('composer').press('enter')
    await until(async () => (await app.getByTestId('completion').count()) === 0)
    await app.getByTestId('send').click()
    await until(async () => (await app.getByTestId('message-record').textContent()).includes('Before'))
    const middle = JSON.parse(await app.getByTestId('message-record').textContent()).parts
    assert.ok(middle.some((p: {path?: string}) => p.path === 'kybern://note/note'))
    assert.ok(middle.some((p: {text?: string}) => p.text === ' after'))
    console.log('CHECK coordinator draft')
    await app.getByTestId('coordinator-draft').click()
    await app.getByTestId('composer').fill('Set up this project with focused checks.')
    await app.screenshot({ path: `${out}/coordinator-draft-${variant}.png` })
    await app.getByTestId('composer').press('enter')
    await until(async () => (await app.getByTestId('rpc-record').textContent()).includes('coordinator.open'))
    assert.equal(JSON.parse(await app.getByTestId('rpc-record').textContent()).params.opening_message, 'Set up this project with focused checks.')
    console.log('CHECK coordinator switch')
    // Enter moves the composer, and the picker button with it. Off macOS a click uses the last painted bounds, which on
    // software renderers can still be the old layout: wait until two reads a frame apart agree before clicking.
    let last = ''
    await until(async () => { const now = JSON.stringify(await app.getByTestId('model-picker').bounds()); const same = now === last; last = now; return same })
    await app.getByTestId('model-picker').click()
    // The picker opens on the next frame or later (software rendering in CI): wait for the tab before clicking it.
    await app.getByTestId('model-tab-codex:default').waitFor()
    await app.getByTestId('model-tab-codex:default').click()
    await until(async () => (await app.getByTestId('rpc-record').textContent()).includes('coordinator.switch_harness'))
    assert.equal(JSON.parse(await app.getByTestId('rpc-record').textContent()).params.provider.kind, 'codex')
    console.log(`PASS ${variant}: structured notes/tasks/Computer, Coordinator opening message and harness switch`)
  } catch (e) { await f.app.screenshot({ path: `${out}/conversation-failed.png` }); console.log((await f.app.call('getPaintedText', {})).text); throw e } finally { console.log(f.diagnostics()); await f.close(); rmSync(scratch, { recursive: true, force: true }) }
}
process.exit(0)
