// PreviewChromeRow.tsx: segmented rest address, edit focus, adaptive actions.
import { useEffect, useRef, useState } from 'react'
import { useGpuix, motion, type PublicInstance } from '@gpuix/react'
import { Alert02Icon, FlashIcon, LinkSquare02Icon, SmartPhone01Icon, PictureInPictureIcon } from '@hugeicons/core-free-icons'
import { glyph, Icon, ICONS } from '../icons.tsx'
import { N } from '../nav/glyphs.ts'
import { Menu, MenuItem, MenuLabel, MenuSeparator, Tip } from '../nav/overlay.tsx'
import { Badge } from '../kit/Badge.tsx'
import { portLabel } from '../../../../packages/kybern-client/src/machineUtilities.ts'
import { usePorts } from './ports.ts'
import { toast } from '../shell/toasts.tsx'
import { errorText } from '../nav/actions.ts'
import { IconButton } from '../kit/IconButton.tsx'
import { Label, REDUCED_MOTION, useTheme } from '../ui.tsx'
import { revealTransform, SwapOnChange } from '../motion/kit.tsx'
import { fadeTo } from '../shell/motion.ts'
import type { ReactNode } from 'react'
import { nativeCapabilities } from '../native/api.ts'
import { PATCHED_NATIVE } from '../native/local-build.ts'
import { webHost } from '../native/webview.ts'
import { observeElementBounds } from '../native/layout.ts'
import { useElementBounds } from '../native/layout-hooks.ts'
import { getState } from '../state.ts'
import { useForwardSources } from './PortForwardBar.tsx'
import { activeEnvironment } from '../shell/environment.ts'
import { copyText } from '../transcript/store.ts'
import { canShowFileInExplorer, showFileInExplorer } from '../dock/explorerSelection.ts'
import { currentAddress } from './address.ts'
import { closePreview, launchPreview, navigatePreview, openPreview, setPreview, setPreviewFast, setPreviewProxy, usePreview } from './store.ts'
import { previewCommand } from './Surface.tsx'
const deviceIcon = glyph(SmartPhone01Icon), floatIcon = glyph(PictureInPictureIcon), externalIcon = glyph(LinkSquare02Icon), warningIcon = glyph(Alert02Icon), fastIcon = glyph(FlashIcon)

export function PreviewChrome({ threadId }: { threadId: string }) {
  const t = useTheme(), s = usePreview(threadId), entry = s.entries[s.index], environment = activeEnvironment()
  const { renderer } = useGpuix(), input = useRef<PublicInstance | null>(null), ports = usePorts(threadId), forwardFrom = useForwardSources()
  const [editing, setEditing] = useState(!entry), [draft, setDraft] = useState(''), [focus, setFocus] = useState(0), [hover, setHover] = useState(false)
  const bounds = useElementBounds('preview-chrome')?.rect, narrow = (bounds?.width ?? 480) < 360
  const thread = getState().threads.find(th => th.id === threadId), project = getState().projects.find(p => p.id === thread?.project_id)
  const address = entry ? currentAddress(entry, s.pagePath, { projectRoot: thread?.cwd ?? project?.path, projectName: project?.name }) : null
  const back = !!(entry?.kind === 'file' && s.bridge?.back) || s.index > 0
  const forward = !!(entry?.kind === 'file' && s.bridge?.forward) || s.index < s.entries.length - 1
  const fileRemote = entry?.kind === 'file' && !environment.local
  const start = () => { setDraft(address?.full ?? ''); setEditing(true); setFocus(n => n + 1); setPreview(threadId, { error: null }) }
  const cancel = () => { setEditing(false); setDraft(''); setPreview(threadId, { error: null }) }
  useEffect(() => { setEditing(!entry); setDraft('') }, [entry])
  useEffect(() => { if (s.focusAddress) { start(); setPreview(threadId, { focusAddress: undefined }) } }, [s.focusAddress])
  useEffect(() => {
    if (!editing || !input.current || !renderer) return
    let focused = false
    return observeElementBounds(renderer, input.current.id, event => {
      if (!event.rect || focused || !input.current) return
      focused = true; renderer.focusElement?.(input.current.id)
      try { nativeCapabilities(renderer).setInputSelection(input.current.id, 0, draft.length, false) } catch { /* host text not committed yet */ }
    })
  }, [renderer, editing, focus])
  const submit = async () => {
    if (!draft.trim()) { cancel(); return }
    if (s.busy) return
    const opened = await openPreview(threadId, draft)
    if (opened) { setEditing(false); setDraft('') }
    else if (input.current) renderer?.focusElement?.(input.current.id)
  }
  const go = (direction: -1 | 1) => {
    if (entry?.kind === 'file' && (direction === -1 ? s.bridge?.back : s.bridge?.forward)) previewCommand(threadId, direction === -1 ? 'back' : 'forward')
    else navigatePreview(threadId, direction)
  }
  const explorerPath = address?.full.split(/[?#]/)[0] ?? ''
  const showExplorer = () => {
    if (entry?.target?.kind !== 'file' || !entry.target.in_project || !thread?.project_id) return
    // Handoff is registered by Explorer without changing preview document ownership.
    showFileInExplorer(threadId, thread.project_id, explorerPath)
  }
  // Device sizes and the mini player drive the embedded page; with no web view the page is in the browser.
  const embedded = webHost.available
  const deviceLabel = s.device ? 'Hide device toolbar' : 'Show device toolbar', floatLabel = s.floating ? 'Return to tab' : 'Float over chat'
  return <div testId="preview-chrome" style={{ display: 'flex', alignItems: 'center', height: 36, flexShrink: 0, gap: 4, paddingLeft: 8, paddingRight: 6, position: 'relative', borderBottomWidth: 1, borderColor: t.surfaceDivider }}>
    <div role="group" aria-label="Navigation" style={{ display: 'flex', gap: 2 }}>
      <Dim on={back}><IconButton testId="preview-back" icon={N.arrowLeft} size="sm" label="Back" onClick={() => back && go(-1)} /></Dim>
      {!narrow && <Dim on={forward}><IconButton testId="preview-forward" icon={N.arrowRight} size="sm" label="Forward" onClick={() => forward && go(1)} /></Dim>}
      <Dim on={!!entry}><ReloadButton loading={s.loading} onClick={() => entry && previewCommand(threadId, s.loading ? 'stop' : 'reload')} /></Dim>
    </div>
    <div onMouseEnter={() => setHover(true)} onMouseLeave={() => setHover(false)} style={{ display: 'flex', alignItems: 'center', flexGrow: 1, minWidth: 0, height: 28, gap: 6, paddingLeft: 8, paddingRight: 8, borderRadius: 8, backgroundColor: editing ? t.v('--color-background-elevated-secondary') : undefined, borderWidth: editing ? 1 : 0, borderColor: t.variant === 'dark' ? 'rgba(115, 115, 115, 0.45)' : 'rgba(163, 163, 163, 0.45)', hover: { backgroundColor: t.buttonSecondary } }}>
      <Icon source={s.load.phase === 'blocked' ? ICONS.shield : s.load.phase === 'waiting' || s.load.phase === 'error' ? warningIcon : entry?.kind === 'file' ? ICONS.file : N.globe} size={14} color={t.textSecondary} />
      {!editing && entry?.kind === 'server' && !environment.local && <Label color={t.textSecondary} size={10}>{environment.name}</Label>}
      {!editing && entry?.machine && <Label color={t.textSecondary} size={10}>{`on ${entry.machine.name}`}</Label>}
      {/* Fast mode (HTTP/2 over Kybern's own certificate): on, it says so here and one click goes back to plain HTTP. */}
      {!editing && entry?.fast?.on && <Tip label={`Loading over HTTP/2${entry.fast.rttMs !== undefined ? ` · ${entry.fast.rttMs} ms to ${entry.machine?.name ?? 'the machine'}` : ''}. Click to use plain HTTP.`}><div><Badge testId="preview-fast" variant="info" size="sm" icon={fastIcon} onClick={() => setPreviewFast(threadId, false)}>Fast</Badge></div></Tip>}
      {editing ? <input ref={input} testId="preview-address" aria-label="Address" value={draft} placeholder="Enter a URL or file path" onChange={e => setDraft(e.value ?? '')} onSubmit={() => void submit()} onBlur={() => { if (!s.busy) cancel() }} onKeyDown={e => { if (e.key === 'escape') cancel() }} style={{ flexGrow: 1, minWidth: 0, height: 26, fontSize: 12, fontFamily: 'JetBrains Mono', color: t.text, backgroundColor: 'transparent' }} />
      : <div testId="preview-address-rest" role="button" aria-label="Address" tabIndex={0} onClick={start} onKeyDown={e => { if (e.key === 'enter' || e.key === 'space') start() }} style={{ display: 'flex', flexGrow: 1, minWidth: 0, overflow: 'hidden', alignItems: 'center', cursor: 'text' }}>
        {address?.segments.length ? address.segments.map((part, i) => <Label key={i} color={part.emphasis && !s.navigatedAway ? t.text : t.textSecondary} size={12} truncate style={{ flexShrink: part.emphasis ? 0 : 1, pointerEvents: 'none' }}>{part.text}</Label>) : <Label color={t.textSecondary} size={12} truncate>Enter a URL or file path</Label>}
      </div>}
      {/* Shown while the bar is hovered: `t-reveal` (150 ms in with a 4 px slide, 100 ms out), inert while hidden. */}
      {entry && !editing && <div paintTransform={revealTransform(hover)} style={{ ...(hover ? {} : { pointerEvents: 'none' }), ...(PATCHED_NATIVE ? {} : { opacity: hover ? 1 : 0 }) }}><div style={{ opacity: fileRemote ? 0.4 : 1 }}><IconButton testId="preview-browser" icon={externalIcon} size="sm" glyph={13} label={fileRemote ? `This file is on ${environment.name}` : 'Open in browser'} onClick={() => !fileRemote && void launchPreview(threadId)} /></div></div>}
    </div>
    {entry && <div style={{ display: 'flex', alignItems: 'center', gap: 2 }}>
      {!narrow && embedded && <><IconButton testId="preview-device" icon={deviceIcon} size="md" label={deviceLabel} active={s.device} onClick={() => setPreview(threadId, { device: !s.device })} /><IconButton testId="preview-float" icon={floatIcon} size="md" label={floatLabel} active={s.floating} onClick={() => entry.kind !== 'external' && setPreview(threadId, { floating: !s.floating })} /></>}
      <Menu align="end" testId="preview-more" minWidth={224} trigger={() => <IconButton icon={N.ellipsis} label="More" size="md" />}>
        {narrow && embedded && <><MenuItem id="device" testId="preview-device" label={deviceLabel} icon={deviceIcon} onSelect={() => setPreview(threadId, { device: !s.device })} /><MenuItem id="float" testId="preview-float" label={floatLabel} icon={floatIcon} disabled={entry.kind === 'external'} onSelect={() => setPreview(threadId, { floating: !s.floating })} /></>}
        {embedded && <MenuItem id="fresh" testId="preview-fresh" label="Reload without cache" icon={N.refresh} onSelect={() => previewCommand(threadId, 'reload', true)} />}
        <MenuItem id="copy" label="Copy address" icon={ICONS.copy} onSelect={() => copyText(address?.full ?? '')} />
        <MenuItem id="browser" label="Open in browser" icon={externalIcon} disabled={fileRemote} onSelect={() => void launchPreview(threadId)} />
        {entry.target?.kind === 'file' && entry.target.in_project && <MenuItem id="explorer" testId="preview-explorer" label="Show file in Explorer" icon={N.folder} disabled={!thread?.project_id || !canShowFileInExplorer(threadId, thread.project_id, explorerPath)} onSelect={showExplorer} />}
        <MenuSeparator />
        {entry.kind === 'server' && environment.local && !entry.fast && <MenuItem id="proxy" testId="preview-proxy" checked={!!entry.proxy} label="Preview through Kybern" onSelect={() => setPreviewProxy(threadId, !entry.proxy)} />}
        {entry.fast && <MenuItem id="fast" testId="preview-fast-toggle" checked={entry.fast.on} label="Fast loading (HTTP/2)" icon={fastIcon} onSelect={() => setPreviewFast(threadId, !entry.fast!.on)} />}
        {environment.local && forwardFrom.length > 0 && <MenuItem id="forward" testId="preview-forward-open" label="Forward a port from…" icon={N.globe} onSelect={() => setPreview(threadId, { forwardOpen: true, forwardMachine: entry.machine?.id })} />}
        {ports.forwards.length > 0 && <>
          <MenuLabel>Forwarded ports</MenuLabel>
          {ports.forwards.map(f => <MenuItem key={f.id} id={`port-${f.id}`} testId={`preview-port-${f.local_port}`} label={portLabel(f)} icon={N.x} onSelect={() => void ports.stop(f.id).catch(e => toast.error('Could not stop the forward', { description: errorText(e) }))} />)}
          <MenuSeparator />
        </>}
        <MenuItem id="close" testId="preview-close" label="Close page" icon={N.x} onSelect={() => closePreview(threadId)} />
      </Menu>
    </div>}
    {s.loading && <motion.div testId="preview-loading" initial={{ width: (bounds?.width ?? 360) * (REDUCED_MOTION ? 0.9 : 0.04) }} animate={{ width: (bounds?.width ?? 360) * 0.9 }} transition={{ duration: REDUCED_MOTION ? 0 : 5.3, ease: [0.1, 0.5, 0.2, 1] }} style={{ position: 'absolute', left: 0, bottom: 0, height: 2, backgroundColor: t.link, borderTopRightRadius: 9999, borderBottomRightRadius: 9999, pointerEvents: 'none' }} />}
  </div>
}

/** A nav button that is unavailable: it eases to 35 % (`transition-opacity duration-150`) instead of switching. */
export function Dim({ on, children }: { on: boolean; children: ReactNode }) {
  const dim = fadeTo(on ? 1 : 0.35)
  return <div paintTransform={dim.paintTransform} style={dim.style}>{children}</div>
}

/** Reload while idle, Stop while a page loads: the glyph swaps like `t-icon-swap` (the old one shrinks and blurs out, the new one grows in). */
export function ReloadButton({ loading, onClick }: { loading: boolean; onClick: () => void }) {
  const t = useTheme()
  return (
    <IconButton testId="preview-reload" label={loading ? 'Stop loading' : 'Reload'} icon={loading ? N.x : N.refresh} size="sm" onClick={onClick}>
      <SwapOnChange kind="icon" active={loading ? 'stop' : 'reload'} style={{ width: 14, height: 14 }}>
        <Icon source={loading ? N.x : N.refresh} size={14} color={t.iconSecondary} style={{ pointerEvents: 'none' }} />
      </SwapOnChange>
    </IconButton>
  )
}
