import { dlopen, FFIType, JSCallback, ptr } from 'bun:ffi'
import { existsSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import path from 'node:path'
import type { LayoutRect, RoundedLayoutClip } from './layout.ts'
import { spawnSync } from 'node:child_process'
import { linuxSandboxProbe, noteWebViewUnavailable, webViewSupport, type LinuxWebViewProbe, type WindowsHostState } from './webview-support.ts'

export type WebEvent = { id: string; generation?: number; type: string; message?: string; url?: string; visible?: boolean; key?: string; modifiers?: { cmd: boolean; ctrl: boolean; alt: boolean; shift: boolean }; data?: Record<string, unknown>
  /** `outsidePress`: window points; `snapshot`: the PNG's pixel size. */
  x?: number; y?: number; width?: number; height?: number; path?: string; ok?: boolean; token?: number
  /** Linux browser UI Kybern draws (`WebPageUi.tsx`): `optionMenu` and `contextMenu` items. */
  items?: WebMenuItem[] }
/** A `<select>` option (`optionMenu`) or a WebKit context menu item (`contextMenu`, with its `index`). */
export type WebMenuItem = { label?: string; enabled?: boolean; selected?: boolean; group?: boolean; child?: boolean; index?: number; separator?: boolean }
/** The host library: `native/webview.m` on macOS, `native/webview-win` (WebView2) on Windows, `native/webview-linux.c` (WPE) on Linux. */
function libraryPath() {
  const name = process.platform === 'win32' ? 'kybern-webview.dll' : process.platform === 'linux' ? 'libkybern-webview.so' : 'kybern-webview.dylib'
  return process.env.KYBERN_GPUIX_RESOURCES ? path.join(process.env.KYBERN_GPUIX_RESOURCES, name) : fileURLToPath(new URL(`../../native/build/${name}`, import.meta.url))
}
/** Windows: the library is there and finds a WebView2 Runtime (`kybern_web_available`), read once at startup. */
function windowsHost(): WindowsHostState {
  const file = libraryPath()
  if (!existsSync(file)) return 'missing'
  try {
    const probe = dlopen(file, { kybern_web_available: { args: [], returns: FFIType.i32 } })
    const ready = probe.symbols.kybern_web_available() === 1
    probe.close()
    return ready ? 'ready' : 'runtime'
  } catch { return 'missing' }
}
/**
 * Decided once: without a host (Linux, Windows without the WebView2 Runtime, or `KYBERN_GPUIX_NO_WEBVIEW=1`) no library
 * stays loaded and every `webHost` call is a no-op. Linux: `linuxSandboxProbe` (pages never run unsandboxed).
 */
const linuxProbe: LinuxWebViewProbe = () => linuxSandboxProbe(libraryPath(), {
  env: process.env,
  exists: existsSync,
  bwrapWorks: bwrap => existsSync(bwrap) && spawnSync(bwrap, ['--unshare-all', '--die-with-parent', '--ro-bind', '/', '/', '--dev', '/dev', '--proc', '/proc', 'true'], { stdio: 'ignore', timeout: 5000 }).status === 0,
})
const SUPPORT = webViewSupport(process.platform, process.env, process.platform === 'win32' ? windowsHost : undefined, process.platform === 'linux' ? linuxProbe : undefined)
noteWebViewUnavailable(SUPPORT.reason, SUPPORT.message)
/** Linux draws documents in GPUIX's scene (`<websurface>`, `WebSurface.tsx`); macOS and Windows float native views over the window. */
const SURFACES = SUPPORT.available && process.platform === 'linux'
/** Where each document was last placed (window pixels), to anchor the menus Kybern draws for it on Linux. */
const placed = new Map<string, LayoutRect>()
let bridge: ReturnType<typeof load> | undefined, wake: JSCallback | undefined
let nextGeneration = 0
let pointerCaptures = 0
/** Documents that were created and not yet destroyed (the picker looks for the live visuals of a thread). */
const live = new Set<string>()
const listeners = new Set<(event: WebEvent) => void>()
const occlusions = new Map<number, { rect: LayoutRect; radius: number; dim: number; order: number; inputOnly: boolean }>()
function load(file: string) {
  return dlopen(file, {
    kybern_web_init: { args: [FFIType.ptr], returns: FFIType.void },
    kybern_web_next: { args: [], returns: FFIType.cstring },
    kybern_web_command: { args: [FFIType.ptr], returns: FFIType.void },
  })
}
let requestId = 0
const pending = new Map<string, { resolve: (value: unknown) => void; reject: (error: Error) => void; timer: ReturnType<typeof setTimeout> }>()
/** One native request, answered by a `frameResult` or `snapshot` event with its `rid`. */
function request(rid: string, send: () => void, timeoutMs = 8000): Promise<unknown> {
  if (!SUPPORT.available) return Promise.reject(new Error("This page can't run here because Kybern has no embedded web view on this platform yet."))
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => { pending.delete(rid); reject(new Error('The page did not answer. Reload it and try again.')) }, timeoutMs)
    pending.set(rid, { resolve, reject, timer })
    try { send() } catch (error) { clearTimeout(timer); pending.delete(rid); reject(error as Error) }
  })
}
function settle(event: WebEvent) {
  const rid = (event as WebEvent & { rid?: string }).rid, entry = rid ? pending.get(rid) : undefined
  if (!rid || !entry) return
  pending.delete(rid); clearTimeout(entry.timer)
  const raw = event as WebEvent & { ok?: boolean; value?: unknown; path?: string; width?: number; height?: number }
  if (raw.ok) entry.resolve(event.type === 'snapshotRect' ? { path: raw.path, width: raw.width, height: raw.height } : raw.value)
  else entry.reject(new Error(event.message || 'The page could not do that.'))
}
function initialize() {
  if (bridge) return
  const file = libraryPath()
  if (!existsSync(file)) throw Error('Build the embedded preview host with bun native/build-webview.ts, then reopen Kybern.')
  const loaded = load(file)
  wake = new JSCallback(() => {
    for (;;) {
      const raw = loaded.symbols.kybern_web_next()
      if (!raw) break
      const event = JSON.parse(String(raw)) as WebEvent
      if (event.type === 'frameResult' || event.type === 'snapshotRect') settle(event)
      for (const listener of listeners) { try { listener(event) } catch (e) { console.error('[webview]', e) } }
    }
  }, { args: [], returns: FFIType.void, threadsafe: true })
  loaded.symbols.kybern_web_init(wake.ptr); bridge = loaded
  for (const [layer, value] of occlusions) command({ op: 'occlusion', id: '', layer, ...value })
  if (pointerCaptures) command({ op: 'capture', id: '', active: true })
}
function command(fields: Record<string, unknown>) {
  if (!SUPPORT.available) return
  initialize()
  const bytes = Buffer.from(JSON.stringify(fields) + '\0')
  bridge!.symbols.kybern_web_command(ptr(bytes))
}

/** Trusted wrapper, opaque sandboxed child. No bearer token or native bridge is
 * exposed to the child. The daemon supplies the child's actual CSP. `send` is WKWebView's message API; on Windows the
 * host defines it in the wrapper over `chrome.webview.postMessage` (`native/webview-win/src/host.rs`).
 * Windows and Linux leave out `allow-modals`: on Windows it is what keeps `print()` from opening WebView2's print UI, and
 * alert, confirm and prompt then return at once, as they do in a WKWebView without a UI delegate; Linux answers them at
 * once too (`on_script_dialog` in `webview-linux.c`), so no page can open a Kybern dialog. */
export function webDocument(url: string, kind: 'file' | 'server' | 'visual', platform: NodeJS.Platform = process.platform) {
  const parsed = new URL(url)
  if (!/^https?:$/.test(parsed.protocol) || parsed.username || parsed.password) throw Error('Invalid preview address')
  const modals = platform === 'win32' || platform === 'linux' ? '' : ' allow-modals'
  const sandbox = kind === 'visual' ? 'allow-scripts' : kind === 'file' ? `allow-scripts allow-forms${modals}` : `allow-scripts allow-same-origin allow-forms${modals} allow-downloads`
  const value = JSON.stringify(url).replaceAll('<', '\\u003c')
  return `<!doctype html><meta name="referrer" content="no-referrer"><meta name="viewport" content="width=device-width, initial-scale=1"><style>html,body{margin:0;width:100%;height:100%;overflow:hidden;background:transparent}iframe{border:0;position:absolute;transform-origin:0 0;width:100%;height:100%}</style><iframe sandbox="${sandbox}" referrerpolicy="no-referrer"></iframe><script>
  const frame=document.querySelector('iframe'), send=data=>window.webkit.messageHandlers.kybern.postMessage(data);
  let visible=true, theme, last='';
  function post(force=false){const data={kind:'kybern-visual-host',visible,theme};const key=JSON.stringify(data);if(force||key!==last){last=key;frame.contentWindow?.postMessage(data,'*');frame.contentWindow?.postMessage({type:'kybern-preview-visibility',visible},'*')}}
  window.kybernVisible=value=>{visible=value;post()};window.kybernTheme=value=>{theme=value;document.documentElement.style.background=value.variables?.['--background']||'transparent';document.documentElement.style.colorScheme=value.appearance==='light'?'light':'dark';post()};
  window.kybernLayout=(w,h,scale,left,top)=>{Object.assign(frame.style,{width:w+'px',height:h+'px',transform:'translate('+left+'px,'+top+'px) scale('+scale+')'})};
  window.kybernSend=data=>frame.contentWindow?.postMessage(data,'*');
  window.kybernCommand=action=>{if(action==='reload')frame.src=frame.src;else if(action==='back'||action==='forward')frame.contentWindow.postMessage({type:'go',delta:action==='back'?-1:1},'*')};
  window.addEventListener('message',event=>{if(event.source!==frame.contentWindow||!event.data||typeof event.data!=='object')return;if(event.data.kind==='kybern-visual-link'&&!navigator.userActivation?.isActive)return;send(event.data)});
  ${kind === 'visual' ? "frame.addEventListener('mouseenter',()=>send({kind:'kybern-visual-hover',value:true}));frame.addEventListener('mouseleave',()=>send({kind:'kybern-visual-hover',value:false}));" : ''}
  frame.onload=()=>{post(true);send({kind:'loaded'})};frame.src=${value};
  </script>`
}

export const webHost = {
  /** False where no embedded web view exists: callers check this and draw their fallback instead of catching errors. */
  available: SUPPORT.available,
  /** Why it is unavailable (`platform`, `disabled` by `KYBERN_GPUIX_NO_WEBVIEW`, Windows `runtime` or `missing`, Linux `sandbox`), else null. */
  unavailableReason: SUPPORT.reason,
  /** Linux: the companion's path, for `<websurface lib>` (the element shares the library bun loaded); null elsewhere. */
  surfaceLibrary: SURFACES ? libraryPath() : null,
  /** The window rect a document was last placed at (Linux menus anchor to it). */
  placement(id: string): LayoutRect | undefined { return placed.get(id) },
  /** A trusted, never-placed document with no child frame (the Mermaid renderer): script runs, the page is not shown. */
  createDocument(id: string, html: string) {
    const generation = ++nextGeneration
    command({ op: 'create', id, category: 'renderer', generation, allowedOrigins: [], html })
    return generation
  },
  /** `trust`: the one certificate a Fast preview's `https://localhost` origin presents (ADE-116). The host accepts exactly it,
   *  (macOS by SHA-256 of the DER for its exact origin; Windows by PEM for its exact origin, remembered per host in the
   *  document's own InPrivate profile; Linux by PEM for its host, for every web view of the process: WPE has no narrower exception);
   *  every other TLS error fails. */
  create(id: string, url: string, kind: 'file' | 'server' | 'visual', trust?: { sha256: string; pem: string }) {
    const generation = ++nextGeneration
    if (!SUPPORT.available) return generation
    const origin = new URL(url), pinned = trust && origin.protocol === 'https:' && ['localhost', '127.0.0.1'].includes(origin.hostname)
    command({ op: 'create', id, category: kind === 'visual' ? 'visual' : 'preview', generation, allowedOrigins: [origin.origin], html: webDocument(url, kind),
      ...(pinned ? { trustedCertificates: [trust.sha256.toLowerCase()], trustedCertificatePems: [trust.pem] } : {}) })
    live.add(id)
    return generation
  },
  /**
   * `slot` names the placeholder showing the document and `slotRect` its box (default `rect`): on Linux the page is
   * painted by that placeholder's `<websurface slot>` at `rect` relative to it, so it follows the placeholder in the
   * same frame while scrolling, under GPUI's own clips and popups.
   */
  place(id: string, rect: LayoutRect, clip: LayoutRect, logical: { width: number; height: number; scale: number; left: number; top: number; radius?: number; roundedClips?: RoundedLayoutClip[] | null; slot?: string; slotRect?: LayoutRect }, visible = true, layer = 0) {
    if (!bridge) return
    placed.set(id, rect)
    const slot = logical.slotRect ?? rect
    command({ op: 'place', id, x: clip.x, y: clip.y, width: clip.width, height: clip.height, offsetX: rect.x - clip.x, offsetY: rect.y - clip.y, logicalWidth: rect.width, logicalHeight: rect.height, visible,
      layer, radius: logical.radius ?? 0, roundedClips: logical.roundedClips ?? [], layout: `window.kybernLayout?.(${[logical.width, logical.height, logical.scale, logical.left, logical.top].join(',')})`,
      slot: logical.slot ?? '', frameX: rect.x - slot.x, frameY: rect.y - slot.y })
  },
  hide(id: string) { if (bridge) command({ op: 'hide', id }) },
  /** Give the keyboard back to GPUI if a document holds it (`native/webview.m` `releaseFocus`); otherwise nothing. */
  releaseFocus() { if (bridge) command({ op: 'release', id: '' }) },
  /** Test seam: the document layer's presented opacity and scale, as a `probe` event. */
  probe(id: string) { if (bridge) command({ op: 'probe', id }) },
  /**
   * Fade and scale the document with its host (N3): Core Animation runs the curve, so the app presents no frames for it.
   * `from` starts an entrance; without it the document eases from what is on screen. `origin` is fractions of its box.
   */
  present(id: string, to: { opacity: number; scale?: number }, motion: { from?: { opacity?: number; scale?: number }; duration?: number; ease?: readonly number[]; origin?: [number, number] } = {}) {
    if (!bridge) return
    command({ op: 'present', id, opacity: to.opacity, scale: to.scale ?? 1, duration: motion.duration ?? 0, ease: motion.ease ? [...motion.ease] : undefined, originX: motion.origin?.[0] ?? 0.5, originY: motion.origin?.[1] ?? 0.5,
      ...(motion.from?.opacity !== undefined ? { fromOpacity: motion.from.opacity } : {}), ...(motion.from?.scale !== undefined ? { fromScale: motion.from.scale } : {}) })
  },
  post(id: string, script: string) { command({ op: 'post', id, script }) },
  /** A message to the page (`bootstrap.js`: `kybern-visual-click`, `kybern-visual-lock`, `kybern-visual-select`). */
  send(id: string, data: Record<string, unknown>) { command({ op: 'post', id, script: `window.kybernSend?.(${JSON.stringify(data)})` }) },
  /** The document as it looks now, written to `path` as a PNG; answered by a `snapshot` event with the same `token`. */
  snapshot(id: string, path: string, token: number) { command({ op: 'snapshot', id, path, token }) },
  theme(id: string, theme: unknown) { command({ op: 'post', id, script: `window.kybernTheme?.(${JSON.stringify(theme)})` }) },
  navigate(id: string, url: string) { command({ op: 'navigate', id, url }) },
  action(id: string, action: 'reload' | 'back' | 'forward' | 'stop') { command({ op: 'action', id, action }) },
  destroy(id: string) { live.delete(id); placed.delete(id); if (bridge) command({ op: 'destroy', id }) },
  /** Linux: answer a page's `<select>` (`optionMenu` event) with an item index, or -1 to dismiss. */
  optionMenu(id: string, index: number) { if (bridge) command({ op: 'optionMenu', id, index }) },
  /** Linux: run a context menu item (`contextMenu` event) by its `index`, or -1 to dismiss. */
  contextMenu(id: string, index: number) { if (bridge) command({ op: 'contextMenu', id, index }) },
  /** Ids of the documents that exist now (created, not destroyed). */
  documents(): string[] { return [...live] },
  /** Give the document the keyboard (picking: Esc is read by the page). */
  focus(id: string) { if (bridge) command({ op: 'focus', id }) },
  /**
   * Run `script` in the page's frame in Kybern's isolated world (`native/webview.m` `frame`): the page shares the DOM but cannot
   * reach this script's globals. Resolves with the script's JSON value; rejects when the page has not loaded or the script throws.
   */
  frame(id: string, script: string): Promise<unknown> {
    const rid = String(++requestId)
    return request(rid, () => command({ op: 'frame', id, script, rid }))
  },
  /** A PNG of a rectangle of the page (CSS pixels in the page's viewport) saved to `path`, which must be under the temp folder. */
  snapshotRect(id: string, rect: { x: number; y: number; width: number; height: number }, path: string): Promise<{ path: string; width: number; height: number }> {
    const rid = String(++requestId)
    return request(rid, () => command({ op: 'snapshotRect', id, rid, path, ...rect })) as Promise<{ path: string; width: number; height: number }>
  },
  capturePointer() {
    if (++pointerCaptures === 1 && bridge) command({ op: 'capture', id: '', active: true })
    let live = true
    return () => { if (!live) return; live = false; if (--pointerCaptures === 0 && bridge) command({ op: 'capture', id: '', active: false }) }
  },
  occlusion(layer: number, rect: LayoutRect | null, radius = 0, dim = 0, order = layer, inputOnly = false) {
    if (rect) occlusions.set(layer, { rect, radius, dim, order, inputOnly }); else occlusions.delete(layer)
    if (bridge) command({ op: 'occlusion', id: '', layer, rect, radius, dim, order, inputOnly })
  },
  subscribe(listener: (event: WebEvent) => void) { listeners.add(listener); return () => { listeners.delete(listener) } },
}
