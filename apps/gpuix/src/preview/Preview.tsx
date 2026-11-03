import { relativeTime } from '@/lib/format'
import { recentIdentity, type PreviewRecent } from './recentSources.ts'
import { Icon, ICONS } from '../icons.tsx'
import { DockSection } from '../dock/Notes.tsx'
import { muted } from '../nav/overlay.tsx'
import { activeEnvironment } from '../shell/environment.ts'
import { useEffect, useState, useSyncExternalStore, type ReactNode } from 'react'
import { VIEWPORT_PRESETS, isViewportSizeValid, parseViewportField } from '../../../../packages/kybern-client/src/previewViewport.ts'
import type { PreviewOpenRequestedNotification, PreviewServersListResult } from '../../../../packages/kybern-client/src/types.ts'
import { getState, localClient, machineOfThreadId, ownProjectId, threadClient, useApp } from '../state.ts'
import { clientFor, useMachines, useMachineView } from '../machines/index.ts'
import { setNav, useNav } from '../nav/store.ts'
import { addWindowKeyHandler, onWindowKeyDown } from '../nav/keys.ts'
import { webHost } from '../native/webview.ts'
import { IconButton } from '../kit/IconButton.tsx'
import { Button } from '../kit/Button.tsx'
import { Badge } from '../kit/Badge.tsx'
import { discoveredServers, projectRootsOnMachines, serverTestId, startServerDiscovery, type DiscoveredServer, type MachineServers, type ServerMachine } from './discovery.ts'
import { PickerChip } from '../kit/PickerChip.tsx'
import { TextField } from '../kit/Field.tsx'
import { Menu, MenuItem } from '../nav/overlay.tsx'
import { N } from '../nav/glyphs.ts'
import { ink, Label, useTheme } from '../ui.tsx'
import { revealTransform } from '../motion/kit.tsx'
import { pressRow } from '../shell/motion.ts'
import { PATCHED_NATIVE } from '../native/local-build.ts'
import { Floating } from '../live/Floating.tsx'
import { DockEmpty, ErrorLine } from '../dock/parts.tsx'
import { disposePreview, floatingPreview, openPreview, openRemoteServer, previewState, previewThreads, previewUses, setPreview, showPreview, subscribePreviews, usePreview } from './store.ts'
import { watchPreviewLifetime } from './lifetime.ts'
import { createPortsStore, type PortsClient } from './ports.ts'
import { PreviewSlot, PreviewSurface, previewCommand } from './Surface.tsx'
import { previewDockSize } from './Surface.tsx'
import { useElementBounds } from '../native/layout-hooks.ts'
import { clampPlayerSize, defaultPlayerSize, type PlayerSize } from '../../../../packages/kybern-client/src/previewMiniPlayer.ts'
import { dockGeometry, useDock } from '../dock/store.ts'
import { subscribeTabs, threadHasTab, useGroups } from '../split/store.ts'
import { useSidebarWidth, useWindowWidth } from '../shell/layout.ts'
import { PreviewResizeCapture } from './resize.tsx'
import { MiniResize, type MiniCorner } from './MiniResize.tsx'
import { MiniChrome } from './MiniChrome.tsx'
import { PreviewChrome } from './Chrome.tsx'
import { PortForwardBar } from './PortForwardBar.tsx'
import { SlowLoadStrip } from './SlowLoad.tsx'
import { addressPath } from './remote.ts'
import { shortMachineName } from '../../../../packages/kybern-client/src/forkMachine.ts'
import { PreviewStateView } from './States.tsx'
import { PictureInPictureIcon } from '@hugeicons/core-free-icons'
import { glyph } from '../icons.tsx'
const PIP = glyph(PictureInPictureIcon)

/** Desktop `DockEmpty` in the preview body: a block parent, so it sits at the top (`py-10`), not centred. */
function FloatingState({ children }: { children: ReactNode }) {
  const t = useTheme()
  return <div testId="preview-state" style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', paddingLeft: 32, paddingRight: 32, paddingTop: 40, paddingBottom: 40 }}>
    <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'center', width: 32, height: 32, borderRadius: 10, marginBottom: 12, backgroundColor: t.v('--color-background-elevated-secondary') }}><Icon source={PIP} size={16} color={muted(t, 0.7)} /></div>
    <Label color={ink(t, 0.8)} size={14} lineHeight={21} weight={500} style={{ textAlign: 'center' }}>Floating over the chat</Label>
    <Label color={muted(t, 0.6)} size={13} lineHeight={16} style={{ textAlign: 'center', marginTop: 4, maxWidth: 256, whiteSpace: 'normal' }}>The page keeps running in the mini player.</Label>
    <div style={{ display: 'flex', marginTop: 12 }}>{children}</div>
  </div>
}
import { forgetRecent, recentsKeyForThread, useRecents } from './recents.ts'
import { isWindowVisible, onWindowVisibility } from '../native/visibility.ts'
import { load, save } from '../shell/persist.ts'
import { isAltGrText, isMacKeys, isPrimary, isPrimaryChord } from '../platform/keys.ts'

export function PreviewPane({ threadId }: { threadId: string }) {
  const t = useTheme(), s = usePreview(threadId)
  const entry = s.entries[s.index]
  const key = recentsKeyForThread(threadId), recents = useRecents(key).slice(0, 8)
  const [visible, setVisible] = useState(isWindowVisible), [now, setNow] = useState(Date.now)
  const [results, setResults] = useState<MachineServers[]>([])
  const [showAll, setShowAll] = useState(false)
  const environment = activeEnvironment()
  const machines = useMachines(), view = useMachineView(), primary = machines.find(m => m.isLocal)
  const home = machineOfThreadId(threadId) ?? primary?.machineId
  const thread = getState().threads.find(thread => thread.id === threadId)
  const roots = projectRootsOnMachines(view.projects, thread ? ownProjectId(thread) : undefined, home)
  const targets: ServerMachine[] = machines.map(machine => ({ ...machine, roots: [...(roots.get(machine.machineId) ?? []), ...(machine.machineId === home && thread?.cwd ? [thread.cwd] : [])], ...(machine.machineId === home ? { threadId } : {}) }))
  // Primitive dependency: unrelated thread streaming and machine-list object identities do not restart scans.
  const targetKey = JSON.stringify(targets.map(({ machineId, name, offline, isLocal, roots, threadId }) => ({ machineId, name, offline, isLocal, roots, threadId })))
  const chosen = machines.find(m => m.machineId === s.serversOn)
  const sorted = discoveredServers(results, chosen?.machineId)
  const shown = showAll ? sorted : sorted.slice(0, 12)
  const statuses = results.filter(result => !chosen || result.machine.machineId === chosen.machineId)
  const searching = statuses.some(result => result.phase === 'loading')
  const unavailable = statuses.filter(result => result.phase === 'offline' || result.phase === 'error')
  const listVisible = !entry && !s.busy && !s.permission && visible
  useEffect(() => onWindowVisibility(setVisible), [])
  useEffect(() => { if (entry || !recents.length || !visible) return; const timer = setInterval(() => setNow(Date.now()), 60000); return () => clearInterval(timer) }, [!!entry, !!recents.length, visible])
  useEffect(() => {
    if (!listVisible) return
    const sources = JSON.parse(targetKey) as ServerMachine[]
    return startServerDiscovery(sources, machine => clientFor(machine.machineId).callRaw('previews.servers.list', machine.threadId ? { thread_id: machine.threadId } : {}, 6000) as Promise<PreviewServersListResult>, setResults)
  }, [targetKey, listVisible])
  useEffect(() => { setShowAll(false) }, [s.serversOn, threadId])
  useEffect(() => { if (s.serversOn && !chosen) setPreview(threadId, { serversOn: undefined }) }, [s.serversOn, chosen?.machineId, threadId])
  const openServer = (server: DiscoveredServer) => void openRemoteServer(threadId, { id: server.machine.machineId, name: shortMachineName(server.machine.name), port: server.port, path: addressPath(server.url), scheme: new URL(server.url).protocol === 'https:' ? 'https' : 'http' })
  return <div testId="dock-preview" style={{ display: 'flex', flexDirection: 'column', flexGrow: 1, minHeight: 0 }}>
    <PreviewChrome threadId={threadId} />
    {entry && s.device && <DeviceToolbar threadId={threadId} />}
    <ErrorLine error={s.error} />
    {s.forwardOpen && <PortForwardBar threadId={threadId} />}
    {entry?.machine && <SlowLoadStrip threadId={threadId} />}
    {!entry && <ServersBar threadId={threadId} chosenId={s.serversOn} />}
    {s.wsFailed && !s.wsNoticeDismissed && entry?.proxy && <div style={{ display: 'flex', alignItems: 'center', gap: 8, paddingLeft: 12, height: 28 }}><Label color={t.textSecondary} size={11} truncate>{`Live reload isn't connected through ${environment.name}. Reload to see changes.`}</Label><Button variant="ghost" size="sm" onClick={() => previewCommand(threadId, 'reload')}>Reload</Button><IconButton icon={N.x} label="Dismiss" size="sm" onClick={() => setPreview(threadId, { wsNoticeDismissed: true })} /></div>}
    {s.permission ? <PreviewStateView threadId={threadId} />
    : s.busy && s.load.phase !== 'waiting' ? <DockEmpty icon={N.globe} title="Opening preview…" />
    : entry ? s.floating ? <FloatingState><Button size="sm" testId="preview-return" onClick={() => setPreview(threadId, { floating: false })}>Return to tab</Button></FloatingState> : s.load.phase !== 'ready' || !webHost.available ? <PreviewStateView threadId={threadId} /> : <PreviewSlot threadId={threadId} kind="dock" />
    : sorted.length || recents.length || unavailable.length ? <virtual-list style={{ flexGrow: 1, minHeight: 0, width: '100%' }}><div style={{ paddingLeft: 12, paddingRight: 12, paddingTop: 16, paddingBottom: 16 }}>
      <DockSection label={chosen ? `Servers on ${shortMachineName(chosen.name)}` : 'Servers on all machines'} count={sorted.length} />
      {searching && <Label color={t.textSecondary} size={12} style={{ paddingLeft: 14, paddingBottom: 8 }}>Looking for servers…</Label>}
      {unavailable.map(result => <Label key={result.machine.machineId} color={t.textSecondary} size={12} lineHeight={18} style={{ paddingLeft: 14, paddingBottom: 8 }}>{`${shortMachineName(result.machine.name)} · ${result.phase === 'offline' ? 'Offline' : 'Could not scan. Retrying…'}`}</Label>)}
      <div style={{ display: 'flex', flexDirection: 'column', marginLeft: 12, marginRight: 12, padding: 4, borderRadius: 10, backgroundColor: t.buttonSecondary }}>
        {shown.map(server => <div key={`${server.machine.machineId}:${server.url}`} testId={serverTestId(server)} role="button" tabIndex={0} onClick={() => openServer(server)} onKeyDown={e => { if (e.key === 'enter') openServer(server) }} style={{ display: 'flex', alignItems: 'center', gap: 10, height: 44, paddingLeft: 10, paddingRight: 10, borderRadius: 6, cursor: 'pointer', hover: { backgroundColor: t.hover }, ...pressRow(t, t.hover) }}>
          <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'center', width: 20, height: 20, borderRadius: 5, backgroundColor: t.v('--color-background-elevated-secondary'), position: 'relative', flexShrink: 0 }}><Icon source={N.globe} size={13} color={t.textSecondary} />{/* `size-1.5 bg-emerald-500 ring-[1.5px] ring-(button-secondary)` */}<div style={{ position: 'absolute', right: -3.5, bottom: -3.5, width: 9, height: 9, borderRadius: 4.5, backgroundColor: t.buttonSecondary, display: 'flex', alignItems: 'center', justifyContent: 'center' }}><div style={{ width: 6, height: 6, borderRadius: 3, backgroundColor: 'oklch(0.696 0.17 162.48)' }} /></div></div>
          <div style={{ display: 'flex', flexDirection: 'column', flexGrow: 1, minWidth: 0 }}><Label color={ink(t, 0.9)} size={14} lineHeight={21} truncate>{server.title && server.title.length <= 48 ? server.title : server.process_name ?? server.host}</Label><Label color={muted(t, 0.7)} size={13} lineHeight={19.5} truncate>{[server.process_name, server.cwd?.split('/').filter(Boolean).slice(-3).join('/')].filter(Boolean).join(' · ')}</Label></div>
          <Badge size="sm" variant="secondary">{shortMachineName(server.machine.name)}</Badge><Label color={t.textSecondary} size={13}>{new URL(server.url).host}</Label>
        </div>)}
      </div>
      {!showAll && sorted.length > 12 && <Button variant="ghost" size="sm" onClick={() => setShowAll(true)}>{`Show ${sorted.length - 12} more`}</Button>}
      {recents.length > 0 && <><DockSection label="Recent" /><div style={{ display: 'flex', flexDirection: 'column', marginLeft: 12, marginRight: 12, padding: 4, borderRadius: 10, backgroundColor: t.buttonSecondary }}>
        {recents.map((recent, index) => <RecentRow key={recentIdentity(recent)} recent={recent} offline={!!recent.machine && (machines.find(machine => machine.machineId === recent.machine!.id)?.offline ?? true)} index={index} now={now} open={() => void (recent.machine ? openRemoteServer(threadId, recent.machine) : openPreview(threadId, recent.value))} forget={() => forgetRecent(key, recent)} />)}
      </div></>}
      <Label color={muted(t, 0.6)} size={13} lineHeight={16} style={{ paddingLeft: 14, paddingTop: 2, paddingBottom: 8 }}>Servers in this project come first. Updated every few seconds.</Label>
    </div></virtual-list> : <DockEmpty icon={N.globe} title={searching || !results.length ? 'Looking for servers…' : chosen ? `No servers running on ${shortMachineName(chosen.name)}` : 'No servers running'} body="Start a dev server or enter a file path above. Servers appear here while they run." />}

  </div>
}
/** Which machine's servers the empty Browser lists, and the way to forward a port by hand. */
function ServersBar({ threadId, chosenId }: { threadId: string; chosenId?: string }) {
  const machines = useMachines(), chosen = machines.find(m => m.machineId === chosenId)
  return <div testId="preview-servers-bar" style={{ display: 'flex', alignItems: 'center', gap: 6, height: 36, flexShrink: 0, paddingLeft: 8, paddingRight: 8 }}>
    <Menu testId="preview-servers-machine" trigger={open => <PickerChip size="sm" open={open}>{chosen ? (chosen.isLocal ? `${shortMachineName(chosen.name)} (this machine)` : shortMachineName(chosen.name)) : 'All machines'}</PickerChip>}>
      <MenuItem id="all" testId="preview-servers-all" label="All machines" checked={!chosenId} onSelect={() => setPreview(threadId, { serversOn: undefined })} />
      {machines.map(m => <MenuItem key={m.machineId} id={m.machineId} testId={`preview-servers-${m.machineId}`} label={`${shortMachineName(m.name)}${m.offline ? ' · Offline' : m.isLocal ? ' (this machine)' : ''}`} checked={m.machineId === chosenId} onSelect={() => setPreview(threadId, { serversOn: m.machineId })} />)}
    </Menu>
    <div style={{ flexGrow: 1 }} />
    <Button size="sm" variant="ghost" testId="preview-forward-open" onClick={() => setPreview(threadId, { forwardOpen: true, forwardMachine: chosenId && chosenId !== machines.find(m => m.isLocal)?.machineId ? chosenId : undefined })}>Forward a port…</Button>
  </div>
}
function RecentRow({ recent, offline, index, now, open, forget }: { recent: PreviewRecent; offline: boolean; index: number; now: number; open: () => void; forget: () => void }) {
  const t = useTheme(), [hover, setHover] = useState(false)
  const title = recent.title ?? (recent.kind === 'file' ? recent.value.split('/').pop() : new URL(recent.value).host) ?? recent.value
  const location = recent.kind === 'file' ? `…/${recent.value.split('/').filter(Boolean).slice(0, -1).slice(-2).join('/')}` : (() => { const url = new URL(recent.value); return `${url.host}${url.pathname === '/' ? '' : url.pathname}` })()
  const relative = relativeTime(new Date(recent.at).toISOString(), now), ago = relative === 'now' ? 'just now' : relative.includes(' ') || /^[A-Z]/.test(relative) ? relative : `${relative} ago`
  return <div onMouseEnter={() => setHover(true)} onMouseLeave={() => setHover(false)} style={{ display: 'flex', alignItems: 'center', height: 44, gap: 8, paddingLeft: 10, paddingRight: 10, borderRadius: 6, hover: { backgroundColor: t.hover } }}>
    <div testId={`preview-recent-${index}`} role="button" tabIndex={0} onClick={open} onFocus={() => setHover(true)} onBlur={() => setHover(false)} onKeyDown={e => { if (e.key === 'enter') open() }} style={{ display: 'flex', flexGrow: 1, minWidth: 0, alignItems: 'center', gap: 10, cursor: 'pointer', borderRadius: 6, ...pressRow(t, t.hover) }}>
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'center', width: 20, height: 20, borderRadius: 5, backgroundColor: t.v('--color-background-elevated-secondary') }}><Icon source={recent.kind === 'file' ? ICONS.file : N.globe} size={13} color={t.textSecondary} /></div><div style={{ display: 'flex', flexDirection: 'column', flexGrow: 1, minWidth: 0 }}><Label color={ink(t, 0.9)} size={14} lineHeight={21} truncate>{title}</Label><Label color={muted(t, 0.7)} size={13} lineHeight={19.5} truncate>{`${recent.machine ? `${recent.machine.name} · ` : ''}${location} · ${offline ? 'Offline' : ago}`}</Label></div>
    </div>{offline && <Button size="sm" variant="ghost" testId={`preview-recent-retry-${index}`} onClick={open}>Retry</Button>}<div paintTransform={revealTransform(hover)} style={{ ...(hover ? {} : { pointerEvents: 'none' }), ...(PATCHED_NATIVE ? {} : { opacity: hover ? 1 : 0 }) }}><IconButton testId={`preview-forget-${index}`} icon={N.x} label={`Remove ${title} from recent`} size="sm" onFocus={() => setHover(true)} onBlur={() => setHover(false)} onClick={forget} /></div>
  </div>
}
function DeviceToolbar({ threadId }: { threadId: string }) {
  const t = useTheme(), s = usePreview(threadId)
  const [width, setWidth] = useState(String(s.width)), [height, setHeight] = useState(String(s.height))
  useEffect(() => { setWidth(String(s.width)); setHeight(String(s.height)) }, [s.width, s.height])
  const commit = () => { const w = parseViewportField(width), h = parseViewportField(height); if (w !== null && h !== null && isViewportSizeValid(w, h)) setPreview(threadId, { width: w, height: h, preset: null, error: null }); else { setWidth(String(s.width)); setHeight(String(s.height)); setPreview(threadId, { error: 'Use dimensions from 240–3840.' }) } }
  return <div testId="preview-device-toolbar" style={{ display: 'flex', height: 32, alignItems: 'center', gap: 4, paddingLeft: 8, paddingRight: 8 }}><Menu testId="preview-device-preset" trigger={open => <PickerChip size="sm" open={open}>{VIEWPORT_PRESETS.find(p => p.id === s.preset)?.label ?? 'Responsive'}</PickerChip>}><MenuItem id="responsive" label="Responsive" onSelect={() => setPreview(threadId, { preset: null })} />{VIEWPORT_PRESETS.map(p => <MenuItem key={p.id} id={p.id} label={`${p.label} · ${p.w} × ${p.h}`} onSelect={() => setPreview(threadId, { width: p.w, height: p.h, preset: p.id })} />)}</Menu><TextField testId="preview-width" label="Width" value={width} onChange={setWidth} onSubmit={commit} onBlur={commit} width={60} style={{ fontVariantNumeric: 'tabular-nums' } as never} /><Label color={t.textSecondary} size={11}>×</Label><TextField testId="preview-height" label="Height" value={height} onChange={setHeight} onSubmit={commit} onBlur={commit} width={60} style={{ fontVariantNumeric: 'tabular-nums' } as never} /><Button variant="ghost" size="sm" testId="preview-rotate" onClick={() => setPreview(threadId, { width: s.height, height: s.width })}>Rotate</Button><IconButton icon={N.x} size="sm" label="Hide device toolbar" onClick={() => setPreview(threadId, { device: false })} /></div>
}
/** Agent requests and shortcut live outside the dock; closing it unmounts its UI. */
export function PreviewIntegration({ insetBottom = 0 }: { insetBottom?: number } = {}) {
  const id = useApp(s => s.selectedId), connected = useApp(s => s.status === 'open')
  const page = useNav(s => s.page), settings = useNav(s => s.settingsOpen)
  const floating = useSyncExternalStore(subscribePreviews, floatingPreview)
  const dock = useDock(id), windowWidth = useWindowWidth(), sidebarWidth = useSidebarWidth(), sidebarOpen = useApp(s => s.sidebarOpen)
  const insetEnd = dock.open && page === 'home' && !settings ? dockGeometry(windowWidth, sidebarOpen ? sidebarWidth : 0, dock.width).width : 0
  useEffect(() => webHost.subscribe(event => { if (event.type === 'shortcut') onWindowKeyDown({ elementId: 0, eventType: 'keyDown', key: event.key, modifiers: event.modifiers }) }), [])
  useEffect(() => addWindowKeyHandler(e => {
    if (!id || page !== 'home' || settings || !isPrimary(e.modifiers)) return
    if (e.key === 'l' && !e.modifiers?.alt) { showPreview(id); setPreview(id, { focusAddress: Date.now() }); return true }
    if (e.key === 'r' && !e.modifiers?.alt) { previewCommand(id, 'reload', !!e.modifiers?.shift); return true }
  }), [id, page, settings])
  useEffect(() => {
    if (!connected) return
    // Requests come from the machine the user is at and from the selected thread's machine.
    const clients = new Set([localClient()!])
    try { clients.add(threadClient(id)!) } catch { /* that machine is offline */ }
    const offs = [...clients].map(client => client.onNotification('previews.open_requested', payload => { const request = payload as PreviewOpenRequestedNotification; showPreview(request.thread_id, true); void openPreview(request.thread_id, request.target) }))
    return () => { for (const off of offs) off() }
  }, [connected, id])
  // A closed browser tab ends its page, and a forward it opened ends 10 s after no browser shows it (ADE-128, `lifetime.ts`).
  useEffect(() => {
    const client = connected ? localClient() : null
    if (!client) return
    const ports = createPortsStore(client as unknown as PortsClient)
    const stop = watchPreviewLifetime({
      threads: previewThreads,
      open: threadId => threadHasTab(threadId, 'preview') || previewState(threadId).floating,
      dispose: disposePreview,
      uses: previewUses,
      forwards: ports.get,
      stop: forwardId => ports.stop(forwardId),
      subscribe: listener => { const offs = [subscribeTabs(listener), subscribePreviews(listener), ports.subscribe(listener)]; return () => { for (const off of offs) off() } },
    })
    return () => { stop(); ports.dispose() }
  }, [connected])
  useEffect(() => addWindowKeyHandler(e => { if (id && page === 'home' && !settings && isPrimaryChord(e.modifiers, { alt: true, shift: 'any' }) && !(!isMacKeys() && isAltGrText(e)) && e.key === 'p') { if (!webHost.available) { showPreview(id); return true } setNav({ keyboardNav: true }); const s = previewState(id); if (s.entries.length) { setPreview(id, { floating: !s.floating }); if (s.floating) showPreview(id) } else showPreview(id); return true } }), [id, page, settings])
  // One native document per thread: the threads whose browser tab is active in some group, the selected one, and the floating one.
  const groups = useGroups()
  const shown = Object.values(groups).filter((g) => g.threadId && g.active === 'preview').map((g) => g.threadId)
  const ids = [...new Set([id, floating, ...shown].filter((value): value is string => !!value))].sort()
  // Without an embedded web view there are no native documents to own or float: the page is in the browser.
  if (!webHost.available) return null
  return <>{ids.map(threadId => <PreviewSurface key={threadId} threadId={threadId} />)}{floating && page === 'home' && !settings ? <MiniPlayer key={floating} insetBottom={insetBottom} insetEnd={insetEnd} threadId={floating} /> : null}<PreviewResizeCapture /></>
}
function MiniPlayer({ threadId, insetBottom, insetEnd }: { threadId: string; insetBottom: number; insetEnd: number }) {
  const t = useTheme(), s = usePreview(threadId), entry = s.entries[s.index]
  const bounds = useElementBounds('preview-mini-bounds')?.rect
  const [corner, setCorner] = useState<MiniCorner>(() => { const saved = load<string>('kybern.preview.corner', 'bottom-end'); return ['top-start','top-end','bottom-start','bottom-end'].includes(saved) ? saved as MiniCorner : 'bottom-end' })
  const [savedSize, setSize] = useState<PlayerSize | null>(() => { const saved = load<PlayerSize | null>('kybern.preview.size', null); return saved && Number.isFinite(saved.w) && Number.isFinite(saved.h) ? saved : null })
  const available = bounds ? { w: Math.max(240, bounds.width - insetEnd), h: bounds.height } : undefined
  const size = savedSize ? clampPlayerSize(savedSize, available) : defaultPlayerSize(previewDockSize(threadId), available)
  if (!s.floating || !entry) return null
  return <Floating insetBottom={insetBottom} insetEnd={insetEnd} defaultCorner="bottom-end" onCornerChange={setCorner} storageKey="kybern.preview.corner" testId="preview-mini" width={size.w} height={size.h} overlayDrag radius={12} cardStyle={MINI_CARD(t)}>
    <MiniBody threadId={threadId} entry={entry} corner={corner} size={size} available={available} setSize={setSize} />
  </Floating>
}
/** kit.css `.preview-surface[data-shape="pip"]`: 12px radius, a heavy-border ring and the two-layer shadow. */
const MINI_CARD = (t: ReturnType<typeof useTheme>) => ({ borderWidth: 1, borderColor: t.borderHeavy, boxShadow: { offsetX: 0, offsetY: 12, blurRadius: 32, spreadRadius: -8, color: t.variant === 'dark' ? 'rgba(0, 0, 0, 0.55)' : 'rgba(0, 0, 0, 0.28)' } } as const)
function MiniBody({ threadId, entry, corner, size, available, setSize }: { threadId: string; entry: NonNullable<ReturnType<typeof usePreview>['entries'][number]>; corner: MiniCorner; size: PlayerSize; available?: PlayerSize; setSize: (size: PlayerSize) => void }) {
  const s = usePreview(threadId)
  const selectedElsewhere = useApp(state => state.selectedId !== threadId), threadTitle = useApp(state => state.threads.find(th => th.id === threadId)?.title)
  // The revealed header takes the pointer from the card (it is its own hit target), so the card's enter/leave alone
  // would flap while the pointer rests on the header; the header reports its own hover and the two are joined.
  const [overBody, setOverBody] = useState(false), [overHeader, setOverHeader] = useState(false), [shown, setShown] = useState(false)
  const hover = overBody || overHeader
  // Hover reveals the header; it stays mounted for its 150ms exit.
  useEffect(() => {
    if (hover) { setShown(true); return }
    const timer = setTimeout(() => setShown(false), 170)
    return () => clearTimeout(timer)
  }, [hover])
  return <div onMouseEnter={() => setOverBody(true)} onMouseLeave={() => setOverBody(false)} style={{ display: 'flex', flexDirection: 'column', flexGrow: 1, minHeight: 0, position: 'relative' }}>
    {s.load.phase === 'ready' && webHost.available ? <PreviewSlot threadId={threadId} kind="mini" /> : <PreviewStateView threadId={threadId} />}
    {(hover || shown) && <MiniChrome title={s.pageTitle || entry.title} otherThread={selectedElsewhere ? threadTitle : undefined} revealed={hover} onHover={setOverHeader}><IconButton icon={N.panelRight} size="sm" testId="preview-mini-return" label="Return to tab" onClick={() => { setPreview(threadId, { floating: false }); showPreview(threadId) }} /><IconButton icon={N.x} size="sm" testId="preview-mini-close" label="Close preview" onClick={() => { setPreview(threadId, { floating: false }) }} /></MiniChrome>}
    <MiniResize corner={corner} size={size} bounds={available} onResize={next => { setSize(next); save('kybern.preview.size', next) }} />
  </div>
}
