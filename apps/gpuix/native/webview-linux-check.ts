// WP10 Linux: the embedded web view drawn by GPUIX (`webview-linux-lab.tsx`), driven through the automation client.
// Under Xvfb + lavapipe (CI) or a desktop session; `KYBERN_CHECK_CAPTURE=none` skips the pixel steps (headless Weston).
//   KYBERN_VERIFY_OUTPUT=<dir> bun native/webview-linux-check.ts
import '../src/native/local-build.ts'
import { existsSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { capture, launch, near, outputDir, pngPixels } from './check-harness.ts'

const output = outputDir('webview-linux')
const pixels = process.env.KYBERN_CHECK_CAPTURE !== 'none'
const results: { name: string; ok: boolean; detail: string }[] = []
const check = (name: string, ok: boolean, detail = '') => { results.push({ name, ok, detail }); console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${detail ? ` — ${detail}` : ''}`) }
const hex = (value: string) => [1, 3, 5].map(i => parseInt(value.slice(i, i + 2), 16))

for (const appearance of ['dark', 'light']) {
  const f = await launch(path.join(import.meta.dir, 'webview-linux-lab.tsx'), { KYBERN_APPEARANCE: appearance })
  const a = f.app, tag = (name: string) => `${appearance} ${name}`
  try {
    await a.getByTestId('app-ready').waitFor()
    const inspect = async () => { await a.getByTestId('action-i').click(); await Bun.sleep(100); return JSON.parse(await a.getByTestId('result').textContent() || '[]') as any[] }
    const until = async (test: (log: any[]) => boolean, ms = 8000) => { const end = Date.now() + ms; let log: any[] = []; while (Date.now() < end) { log = await inspect(); if (test(log)) return log; await Bun.sleep(250) } return log }
    let log = await until(l => l.some(e => e.data?.kind === 'kybern-visual-ready'), 20000)
    check(tag('page loads and the bridge reaches the host'), log.some(e => e.type === 'loaded') && log.some(e => e.data?.kind === 'kybern-visual-ready'), f.diagnostics?.() ?? '')
    check(tag('the sandboxed child cannot post host messages'), !log.some(e => e.data?.kind === 'forged'))
    log = await until(l => l.some(e => e.data?.kind === 'host-state' && e.data.appearance === appearance))
    check(tag('host → page message round trip (theme, visibility)'), log.some(e => e.data?.kind === 'host-state' && e.data.appearance === appearance && e.data.visible === true))
    await Bun.sleep(500)
    const slot = await a.getByTestId('web-slot').bounds(), clip = await a.getByTestId('web-clip').bounds()
    const at = (x: number, y: number) => ({ x: slot.x + x, y: slot.y + y })
    if (pixels) {
      const shot = await capture(f, path.join(output, `${appearance}-page.png`))
      const png = pngPixels(shot), scale = png.width / 780
      const px = (p: { x: number; y: number }) => png.at(Math.round(p.x * scale), Math.round(p.y * scale))
      const clipBg = hex(appearance === 'light' ? '#e2e6f5' : '#2c3350'), windowBg = hex(appearance === 'light' ? '#f2f2f2' : '#242424')
      check(tag('the page paints (translucent card over the clip background)'), !near(px(at(170, 70)), clipBg, 12), `card ${px(at(170, 70))} vs clip ${clipBg}`)
      check(tag('transparent page: GPUI content shows through'), near(px(at(450, 270)), clipBg, 12), `${px(at(450, 270))} vs ${clipBg}`)
      check(tag('rounded clip cuts the page corner'), near(px({ x: clip.x + 2, y: clip.y + 2 }), windowBg, 12), `${px({ x: clip.x + 2, y: clip.y + 2 })} vs ${windowBg}`)
      await a.getByTestId('action-p').click(); await Bun.sleep(300)
      const menuShot = await capture(f, path.join(output, `${appearance}-popup.png`)), menu = pngPixels(menuShot)
      const popupBg = hex(appearance === 'light' ? '#fafafa' : '#303030')
      check(tag('a GPUIX popup draws above the page'), near(menu.at(Math.round(200 * scale), Math.round(250 * scale)), popupBg, 12), `${menu.at(Math.round(200 * scale), Math.round(250 * scale))}`)
      await a.getByTestId('action-p').click(); await Bun.sleep(200)
    }
    await a.mouse.click(at(94, 160))
    log = await until(l => l.some(e => e.data?.value?.clicks === 1))
    check(tag('a click in the page drafts an answer'), log.some(e => e.type === 'message' && e.data?.kind === 'kybern-visual-draft' && e.data.value?.clicks === 1))
    await a.mouse.click(at(265, 160))
    const menuShown = await a.getByTestId('web-option-menu').waitFor({ timeoutMs: 5000 }).then(() => true, () => false)
    check(tag('<select> opens a Kybern-drawn menu'), menuShown)
    if (menuShown) {
      if (pixels) { await Bun.sleep(400); await capture(f, path.join(output, `${appearance}-select.png`)) }
      await a.getByText('Blue').click()
      log = await until(l => l.some(e => e.data?.value?.color === 'Blue'))
      check(tag('picking from the Kybern menu changes the <select>'), log.some(e => e.data?.value?.color === 'Blue'))
    }
    await a.mouse.click(at(100, 215)); await Bun.sleep(200)
    await a.call('keystrokes', { keys: 'h i' })
    log = await until(l => l.some(e => e.data?.value?.text === 'hi'))
    check(tag('typing reaches a field in the page'), log.some(e => e.data?.value?.text === 'hi'), JSON.stringify(log.filter(e => e.data?.value?.text !== undefined).map(e => e.data.value.text)))
    const snapshot = path.join(tmpdir(), 'kybern-wp10-lab-snapshot.png')
    await a.getByTestId('action-s').click()
    log = await until(l => l.some(e => e.type === 'snapshot'))
    const snap = log.find(e => e.type === 'snapshot')
    check(tag('snapshot writes the page as a PNG at device scale'), !!snap?.ok && existsSync(snapshot) && snap.width >= 560, JSON.stringify(snap))
    await a.getByTestId('action-e').click(); await Bun.sleep(500)
    if (pixels) await capture(f, path.join(output, `${appearance}-presented.png`))
    await a.getByTestId('action-h').click()
    log = await until(l => l.some(e => e.type === 'visibility' && e.visible === false))
    check(tag('hide stops painting and tells the page'), log.some(e => e.type === 'visibility' && e.visible === false))
    if (pixels) {
      await Bun.sleep(300)
      const hidden = pngPixels(await capture(f, path.join(output, `${appearance}-hidden.png`))), scale = hidden.width / 780
      check(tag('a hidden page leaves the placeholder'), near(hidden.at(Math.round(at(170, 70).x * scale), Math.round(at(170, 70).y * scale)), hex(appearance === 'light' ? '#e2e6f5' : '#2c3350'), 12))
    }
    // ADE-116 Fast preview: the pinned certificate loads only in the preview that pins it.
    await a.getByTestId('action-t').click()
    const tlsOk = (l: any[], id: string) => l.some(e => e.id === id && e.data?.kind === 'tls-ok')
    log = await until(l => tlsOk(l, 'fast'), 15000)
    check(tag('a Fast preview loads its pinned certificate'), tlsOk(log, 'fast'), JSON.stringify(log.filter(e => e.id === 'fast')))
    await Bun.sleep(8000)
    log = await inspect()
    check(tag('a pinned preview still refuses a different certificate on loopback'), !tlsOk(log, 'plain') && log.some(e => e.id === 'plain'), JSON.stringify(log.filter(e => e.id === 'plain')))
    writeFileSync(path.join(output, `${appearance}-events.json`), JSON.stringify(await inspect(), null, 1))
  } catch (error) {
    check(tag('run'), false, String(error))
  } finally {
    await f.close?.()
  }
}
writeFileSync(path.join(output, 'summary.json'), JSON.stringify(results, null, 2))
const failed = results.filter(r => !r.ok)
console.log(`webview-linux: ${results.length - failed.length}/${results.length} passed`)
process.exit(failed.length ? 1 : 0)
