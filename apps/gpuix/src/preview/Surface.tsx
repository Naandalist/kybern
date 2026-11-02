import { useEffect, useRef, useState } from 'react'
import { useGpuixRequired, type PublicInstance } from '@gpuix/react'
import { observeElementBounds, type ElementBoundsEvent } from '../native/layout.ts'
import { webHost } from '../native/webview.ts'
import { WebSurface } from '../native/WebSurface.tsx'
import { viewportLayout } from '../../../../packages/kybern-client/src/previewViewport.ts'
import { validateBridgeMessage } from '../../../../packages/kybern-client/src/previewTarget.ts'
import { useTheme } from '../ui.tsx'
import { launchPreview, openPreview, previewState, releasePreviewEntry, setPreview, usePreview, type Entry } from './store.ts'
import { recordRecent, recentsKeyForThread } from './recents.ts'
import { currentAddress, reloadAddress } from './address.ts'
import { DeviceRails } from './DeviceRails.tsx'
import { webDocumentEnter } from '../motion/kit.tsx'

type Slot = { kind: 'dock' | 'mini'; bounds: ElementBoundsEvent }
const slots = new Map<string, Map<string, Slot>>(), placements = new Map<string, () => void>()
const dockedSizes = new Map<string, { w: number; h: number }>()
export const previewDockSize = (id: string) => dockedSizes.get(id) ?? { w: 16, h: 10 }
export const previewDocumentId = (id: string) => `preview:${id}`

/** Slots only publish geometry. Dock/mini switches never move or recreate a web view. */
export function PreviewSlot({ threadId, kind }: { threadId: string; kind: 'dock' | 'mini' }) {
  const renderer = useGpuixRequired(), ref = useRef<PublicInstance | null>(null)
  const [size, setSize] = useState({ width: 0, height: 0 })
  useEffect(() => {
    if (!ref.current) return
    const key = `${kind}:${ref.current.id}`
    const stop = observeElementBounds(renderer, ref.current.id, bounds => {
      let entries = slots.get(threadId)
      if (!entries) slots.set(threadId, entries = new Map())
      entries.set(key, { kind, bounds }); placements.get(threadId)?.()
      if (kind === 'dock' && bounds.rect) { const { width, height } = bounds.rect; setSize(previous => previous.width === width && previous.height === height ? previous : { width, height }) }
    })
    return () => { stop(); slots.get(threadId)?.delete(key); if (!slots.get(threadId)?.size) slots.delete(threadId); placements.get(threadId)?.() }
  }, [renderer, threadId, kind])
  return <div ref={ref} testId={`preview-slot-${kind}`} style={{ flexGrow: 1, minHeight: 0, width: '100%', position: 'relative' }}>{kind === 'dock' && <DeviceRails threadId={threadId} width={size.width} height={size.height} />}<WebSurface id={previewDocumentId(threadId)} slot={kind} /></div>
}

/** Matching PreviewSurfaceLayer: one selected dock document and one floating
 * document. Components are keyed by thread, independent of either slot. */
export function PreviewSurface({ threadId }: { threadId: string }) {
  const s = usePreview(threadId), t = useTheme(), id = previewDocumentId(threadId), entry = s.entries[s.index]
  const live = useRef(s), ready = useRef(false)
  const hiddenTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined), suspended = useRef<Entry | null>(null)
  const visibility = useRef<(visible: boolean) => void>(() => {})
  live.current = s
  useEffect(() => {
    visibility.current = visible => {
      if (visible || live.current.floating) { clearTimeout(hiddenTimer.current); hiddenTimer.current = undefined; return }
      if (hiddenTimer.current || !ready.current) return
      hiddenTimer.current = setTimeout(() => {
        hiddenTimer.current = undefined
        const current = live.current, entry = current.entries[current.index]
        if (!entry || current.floating) return
        suspended.current = entry; ready.current = false; webHost.destroy(id); releasePreviewEntry(entry)
      }, 5 * 60_000)
    }
    const place = () => {
      const current = live.current, kind = current.floating ? 'mini' : 'dock'
      const slot = [...(slots.get(threadId)?.values() ?? [])].find(value => value.kind === kind)
      const rect = slot?.bounds.rect, clip = slot?.bounds.clippedRect
      if (!rect || !clip || current.busy || current.load.phase !== 'ready') { if (ready.current) webHost.hide(id); visibility.current(false); return }
      visibility.current(true)
      if (suspended.current) { const entry = suspended.current; suspended.current = null; void openPreview(threadId, currentAddress(entry, current.pagePath).full, false, false, true); return }
      if (!ready.current) return
      if (kind === 'dock') { dockedSizes.delete(threadId); dockedSizes.set(threadId, { w: rect.width, h: rect.height }); if (dockedSizes.size > 40) dockedSizes.delete(dockedSizes.keys().next().value!) }
      const logical = current.device ? { w: current.width, h: current.height } : kind === 'mini' ? dockedSizes.get(threadId) ?? { w: 512, h: 320 } : { w: rect.width, h: rect.height }
      const scale = Math.min(1, rect.width / logical.w, rect.height / logical.h)
      const fit = kind === 'mini' ? { scale, left: (rect.width - logical.w * scale) / 2, top: (rect.height - logical.h * scale) / 2 } : current.device ? viewportLayout(logical, { w: rect.width, h: rect.height }) : { scale: 1, left: 0, top: 0 }
      const frame = { x: rect.x + fit.left, y: rect.y + fit.top, width: logical.w * fit.scale, height: logical.h * fit.scale }
      const x = Math.max(frame.x, clip.x), y = Math.max(frame.y, clip.y)
      const clipped = { x, y, width: Math.max(0, Math.min(frame.x + frame.width, clip.x + clip.width) - x), height: Math.max(0, Math.min(frame.y + frame.height, clip.y + clip.height) - y) }
      webHost.place(id, frame, clipped, { width: logical.w, height: logical.h, scale: fit.scale, left: 0, top: 0, radius: kind === 'mini' ? 12 : 0, roundedClips: slot?.bounds.roundedClips, slot: kind, slotRect: rect })
    }
    placements.set(threadId, place); place()
    return () => { clearTimeout(hiddenTimer.current); placements.delete(threadId); webHost.hide(id) }
  }, [threadId, id])
  useEffect(() => {
    if (!entry || entry.kind === 'external' || s.load.phase !== 'ready') return
    if (entry.released) { void openPreview(threadId, currentAddress(entry, live.current.pagePath).full, false, false, true); return }
    let alive = true, generation = 0
    const stop = webHost.subscribe(event => {
      if (event.id !== id || !alive || (event.generation !== undefined && event.generation !== generation)) return
      if (event.type === 'created') webDocumentEnter(id, webHost.present)
      if (event.type === 'created' || event.type === 'loaded') { ready.current = true; placements.get(threadId)?.(); webHost.theme(id, { appearance: t.variant, variables: { '--background': t.surface, '--foreground': t.text } }) }
      if (event.type === 'failed' || event.type === 'terminated') setPreview(threadId, { loading: false, load: { phase: 'error', message: event.message ?? 'The page stopped responding. Reload to try again.' } })
      if (event.type === 'navigation') setPreview(threadId, { loading: false, navigatedAway: entry.kind === 'server' && event.url !== (entry.frameUrl ?? entry.url) })
      if (event.type === 'visibility') visibility.current(!!event.visible)
      if (event.type === 'externalNavigation') setPreview(threadId, { error: 'This link opens outside the preview. Use Open in browser to continue.' })
      if (event.type === 'message') {
        const message = validateBridgeMessage(event.data)
        if (event.data?.kind === 'loaded') setPreview(threadId, { loading: false })
        if (message?.type === 'nav-start') setPreview(threadId, { loading: true })
        if (entry.kind === 'file' && message && message.type !== 'nav-start') {
          if (message.title && message.title !== live.current.pageTitle) recordRecent(recentsKeyForThread(threadId), { kind: 'file', value: entry.address, title: message.title, at: Date.now() })
          setPreview(threadId, { loading: false, bridge: { back: message.back, forward: message.forward }, pageTitle: message.title, pagePath: message.path })
        }
      }
    })
    try { generation = webHost.create(id, entry.frameUrl ?? entry.url, entry.kind, entry.trust) }
    catch (error) { setPreview(threadId, { error: String(error) }) }
    return () => { alive = false; ready.current = false; clearTimeout(hiddenTimer.current); hiddenTimer.current = undefined; stop(); webHost.destroy(id); releasePreviewEntry(entry) }
  }, [id, threadId, entry])
  useEffect(() => { placements.get(threadId)?.() }, [s.floating, s.device, s.width, s.height, s.busy, s.load.phase])
  useEffect(() => { if (ready.current) webHost.theme(id, { appearance: t.variant, variables: { '--background': t.surface, '--foreground': t.text } }) }, [id, t])
  useEffect(() => {
    if (!s.loading) return
    const timer = setTimeout(() => setPreview(threadId, { loading: false }), 15000)
    return () => clearTimeout(timer)
  }, [threadId, s.loading])
  return null
}

export function previewCommand(threadId: string, action: 'reload' | 'back' | 'forward' | 'stop', fresh = false) {
  const s = previewState(threadId), entry = s.entries[s.index]
  if (!entry) return
  // The page lives in the browser: Reload opens it there again; the other commands have nothing to drive.
  if (!webHost.available) { if (action === 'reload') void launchPreview(threadId); else setPreview(threadId, { loading: false }); return }
  if (action === 'stop') { webHost.action(previewDocumentId(threadId), action); setPreview(threadId, { loading: false }); return }
  if (action === 'reload') {
    if (s.load.phase !== 'ready' || entry.released || (fresh && entry.kind === 'file')) {
      const address = currentAddress(entry, s.pagePath).full
      void openPreview(threadId, address, false, false, true, { suffix: /[?#].*$/.exec(address)?.[0] })
      return
    }
    setPreview(threadId, { loading: true })
    webHost.navigate(previewDocumentId(threadId), reloadAddress(entry, s.pagePath, fresh))
    return
  }
  setPreview(threadId, { loading: true }); webHost.action(previewDocumentId(threadId), action)
}
