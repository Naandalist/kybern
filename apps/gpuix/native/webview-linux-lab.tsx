// WP10 Linux lab: one live visual drawn by GPUIX's `<websurface>` from WPE WebKit (`webview-linux.c`), inside a
// rounded clip, with a GPUIX popup that can cover it and the browser UI Kybern draws (`WebPageUi`). Driven by
// `webview-linux-check.ts` through the automation client; the result line shows the event log on demand.
import '../src/native/local-build.ts'
import { render, useGpuixRequired, type PublicInstance } from '@gpuix/react'
import { useEffect, useRef, useState } from 'react'
import { webDocument, webHost, type WebEvent } from '../src/native/webview.ts'
import { WebSurface } from '../src/native/WebSurface.tsx'
import { WebPageUi } from '../src/native/WebPageUi.tsx'
import { observeElementBounds } from '../src/native/layout.ts'
import { PopupLayer } from '../src/composer/Menu.tsx'
import { ThemeProvider } from '../src/ui.tsx'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { mkdtempSync, readFileSync } from 'node:fs'
import { X509Certificate } from 'node:crypto'

const light = process.env.KYBERN_APPEARANCE === 'light'
const log: WebEvent[] = []
// A visual as the daemon serves it: transparent page, the visuals CSP sandbox, bootstrap-style messages to the host.
const page = `<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1"><style>
html,body{margin:0;background:transparent;font:15px system-ui;color:${light ? '#161616' : '#fafafa'}}
#card{position:absolute;left:24px;top:24px;width:300px;height:96px;border-radius:18px;background:rgba(67,92,202,.55)}
#go{position:absolute;left:24px;top:140px;width:140px;height:40px;font:inherit;border:0;border-radius:8px;background:#435cca;color:#fff}
#pick{position:absolute;left:190px;top:144px;width:150px;height:32px;font:inherit}
#field{position:absolute;left:24px;top:200px;width:240px;height:30px;font:inherit}
</style><div id=card></div><button id=go>Draft answer</button>
<select id=pick><option>Red</option><option>Green</option><option>Blue</option></select><input id=field placeholder="Type here">
<script>
const up=d=>parent.postMessage(d,'*');let count=0;
up({kind:'kybern-visual-ready'});
go.onclick=()=>up({kind:'kybern-visual-draft',value:{clicks:++count}});
pick.onchange=()=>up({kind:'kybern-visual-draft',value:{color:pick.value}});
field.oninput=()=>up({kind:'kybern-visual-draft',value:{text:field.value}});
addEventListener('message',e=>{if(e.source===parent&&e.data&&e.data.kind==='kybern-visual-host')up({kind:'host-state',visible:e.data.visible,appearance:e.data.theme&&e.data.theme.appearance})});
try{window.webkit.messageHandlers.kybern.postMessage({kind:'forged'})}catch(_){}
</script>`
const server = Bun.serve({ hostname: '127.0.0.1', port: 0, fetch: () => new Response(page, { headers: { 'content-type': 'text/html', 'content-security-policy': "sandbox allow-scripts; default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'" } }) })
const url = `http://127.0.0.1:${server.port}/visual`
const ID = 'lab'

// A Fast preview (ADE-116): an HTTPS loopback page whose self-signed certificate the preview pins. A second server on another
// port presents a different certificate: a preview pinning the first one must still refuse it.
function tlsServer() {
  const dir = mkdtempSync(path.join(tmpdir(), 'kybern-lab-tls-')), key = path.join(dir, 'key.pem'), cert = path.join(dir, 'cert.pem')
  const made = Bun.spawnSync(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', key, '-out', cert, '-days', '1',
    '-subj', '/CN=localhost', '-addext', 'subjectAltName=DNS:localhost,IP:127.0.0.1'])
  if (made.exitCode !== 0) return null
  const pem = readFileSync(cert, 'utf8')
  const fast = `<!doctype html><script>parent.postMessage({kind:'tls-ok'},'*')</script>`
  const tls = Bun.serve({ hostname: '127.0.0.1', port: 0, tls: { cert: pem, key: readFileSync(key, 'utf8') }, fetch: () => new Response(fast, { headers: { 'content-type': 'text/html' } }) })
  return { tls, url: `https://127.0.0.1:${tls.port}/`, trust: { sha256: new X509Certificate(pem).fingerprint256.replaceAll(':', ''), pem } }
}
let pinned: ReturnType<typeof tlsServer> | undefined
const others: { stop(): void }[] = []

function Lab() {
  const renderer = useGpuixRequired(), slot = useRef<PublicInstance | null>(null)
  const [popup, setPopup] = useState(false), [result, setResult] = useState('')
  useEffect(() => {
    const stop = webHost.subscribe(event => { log.push(event); if (event.type === 'loaded') webHost.theme(ID, { appearance: light ? 'light' : 'dark', variables: {} }) })
    webHost.create(ID, url, 'visual')
    return () => { stop(); webHost.destroy(ID); webHost.destroy('fast'); webHost.destroy('plain'); server.stop(); pinned?.tls.stop(); for (const tls of others) tls.stop() }
  }, [])
  useEffect(() => {
    if (!slot.current) return
    let apply = () => {}
    const stop = observeElementBounds(renderer, slot.current.id, event => {
      apply = () => { const r = event.rect, c = event.clippedRect; if (!r || !c) { webHost.hide(ID); return } webHost.place(ID, r, c, { width: r.width, height: r.height, scale: 1, left: 0, top: 0, roundedClips: event.roundedClips }) }
      apply()
    })
    const events = webHost.subscribe(e => { if (e.id === ID && (e.type === 'created' || e.type === 'loaded')) apply() })
    return () => { stop(); events() }
  }, [renderer])
  const act = (key: string) => {
    if (key === 'p') setPopup(n => !n)
    if (key === 'i') setResult(JSON.stringify(log))
    if (key === 's') webHost.snapshot(ID, path.join(tmpdir(), 'kybern-wp10-lab-snapshot.png'), 1)
    if (key === 'e') webHost.present(ID, { opacity: 1, scale: 1 }, { from: { opacity: 0, scale: 0.96 }, duration: 0.3, ease: [0.23, 1, 0.32, 1], origin: [0.5, 0.5] })
    if (key === 'h') webHost.hide(ID)
    if (key === 't' && pinned === undefined) {
      pinned = tlsServer()
      if (!pinned) log.push({ id: 'fast', type: 'failed', message: 'openssl is missing' } as unknown as WebEvent)
      else {
        webHost.create('fast', pinned.url, 'server', pinned.trust)
        const other = tlsServer()
        if (other) {
          others.push(other.tls)
          setTimeout(() => pinned && webHost.create('plain', other.url, 'server', pinned.trust), 3000)
        }
      }
    }
  }
  const bg = light ? '#f2f2f2' : '#242424', fg = light ? '#111' : '#eee'
  return <div testId="app-ready" style={{ width: '100%', height: '100%', display: 'flex', flexDirection: 'column', backgroundColor: bg }}>
    <text style={{ color: fg, height: 40, padding: 12 }}>WP10 Linux web view lab (WPE → GPUIX texture)</text>
    <div style={{ display: 'flex', gap: 10, paddingLeft: 16, height: 34 }}>{[['p', 'Menu'], ['i', 'Inspect'], ['s', 'Snapshot'], ['e', 'Present'], ['h', 'Hide'], ['t', 'TLS']].map(([key, label]) =>
      <div key={key} testId={`action-${key}`} onClick={() => act(key!)} style={{ width: 96, height: 28, backgroundColor: light ? '#ddd' : '#444', borderRadius: 6 }}><text style={{ color: fg, pointerEvents: 'none' }}>{label}</text></div>)}</div>
    <div testId="web-clip" style={{ position: 'absolute', left: 20, top: 100, width: 560, height: 300, borderRadius: 28, overflow: 'hidden', backgroundColor: light ? '#e2e6f5' : '#2c3350' }}>
      <div ref={slot} testId="web-slot" style={{ width: '100%', height: '100%', position: 'relative' }}><WebSurface id={ID} /></div>
    </div>
    {popup && <PopupLayer position={{ x: 100, y: 150 }} testId="web-menu" style={{ width: 240, height: 120, padding: 16, backgroundColor: light ? '#fafafa' : '#303030', borderRadius: 12 }}><text style={{ color: fg }}>A GPUIX popup drawn over the page.</text></PopupLayer>}
    <text testId="result" style={{ position: 'absolute', left: 10, top: 420, width: 760, height: 180, fontSize: 9, color: fg }}>{result}</text>
    <WebPageUi />
  </div>
}
render(<ThemeProvider><Lab /></ThemeProvider>, { title: 'Kybern WP10 Linux web view lab', width: 780, height: 620, focus: false })
