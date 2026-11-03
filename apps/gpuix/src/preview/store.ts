import { useSyncExternalStore } from 'react'
import { classifyPreviewInput, PREVIEW_REJECTED_MESSAGE } from '../../../../packages/kybern-client/src/previewTarget.ts'
import { httpBase } from '../../../../packages/kybern-client/src/address.ts'
import type { PortForwardOrigin, PreviewOpenResult, PreviewTargetInfo, PreviewProbeResult } from '../../../../packages/kybern-client/src/types.ts'
import { getState, localClient, machineOfThreadId, threadClient } from '../state.ts'
import { launchBrowser } from './browser.ts'
import { openTab } from '../tabs/open.ts'
import { openRelay } from './relay.ts'
import { activeEnvironment } from '../shell/environment.ts'
import { recordRecent, recentsKeyForThread } from './recents.ts'
import { browserAddress, currentAddress } from './address.ts'
import { webHost } from '../native/webview.ts'
import { addressPath, forwardedAddress, forwardPort, localServerOn, sourceAddress, type RemoteServer } from './remote.ts'
import { fastAddress, fastKey, wantsFast, type FastMode } from './fast.ts'
import { machineHub, machineRows } from '../machines/index.ts'
import { previewUse, type PreviewUse } from './lifetime.ts'
import { shortMachineName } from '../../../../packages/kybern-client/src/forkMachine.ts'
export type PreviewLoad = { phase: 'idle' | 'resolving' | 'ready' | 'external' } | { phase: 'waiting'; error: 'connection_refused' | 'timed_out'; startedAt: number } | { phase: 'blocked'; header: string; value: string } | { phase: 'error'; error?: string; status?: number; message?: string } | { phase: 'not-found'; name: string } | { phase: 'relay-failed' }
export interface Entry { target?: PreviewTargetInfo; proxy?: boolean; query?: string; address: string; url: string; frameUrl?: string; closeRelay?: () => void; title: string; ticket?: string; kind: 'file' | 'server' | 'external'; /** A dev server on another machine, shown through a forward of its port. */ machine?: RemoteServer; releaseTicket?: () => void; released?: boolean
  /** Fast mode (ADE-116): whether the page loads over the forward's HTTP/2 listener, and the round trip that decided it. */ fast?: FastMode
  /** The certificate the web view accepts for a Fast page (`https://localhost`): only this one, only for its origin. */ trust?: { sha256: string; pem: string } }
export interface PreviewState { entries: Entry[]; load: PreviewLoad; loading: boolean; wsFailed?: boolean; wsNoticeDismissed?: boolean; attemptedAddress?: string; attemptedMachine?: RemoteServer; navigatedAway?: boolean; index: number; floating: boolean; busy: boolean; error: string | null; permission?: { address: string; folder: string; grantable: boolean }; opened?: boolean; /** The machine whose servers the empty Browser lists (all machines when unset). */ serversOn?: string; /** The "Forward a port" strip is open. */ forwardOpen?: boolean; forwardMachine?: string; device: boolean; width: number; height: number; preset: string | null; bridge?: { back: number; forward: number }; pageTitle?: string; pagePath?: string; focusAddress?: number
  /** Fast mode chosen by hand for a server (`fastKey`): true for HTTP/2, false for plain HTTP. Unset follows the round trip. */ fastChoice?: Record<string, boolean> }
const empty: PreviewState = { entries: [], load: { phase: 'idle' }, loading: false, index: -1, floating: false, opened: false, busy: false, error: null, device: false, width: 390, height: 844, preset: 'iphone-12-pro' }
const sessions = new Map<string, PreviewState>(), generations = new Map<string, number>()
let nextGeneration = 0
const listeners = new Set<() => void>()
export const previewState = (id: string) => sessions.get(id) ?? empty
export const usePreview = (id: string) => useSyncExternalStore(l => { listeners.add(l); return () => { listeners.delete(l) } }, () => previewState(id))
export const subscribePreviews = (listener: () => void) => { listeners.add(listener); return () => { listeners.delete(listener) } }
/** Threads that hold browser state, and what each one's browser shows (the forwards it keeps open, `lifetime.ts`). */
export const previewThreads = () => [...sessions.keys()]
export const previewUses = (): PreviewUse[] => [...sessions.values()].map(previewUse)
export const floatingPreview = () => [...sessions].find(([, s]) => s.floating && s.entries.length)?.[0] ?? null
export const releasePreviewEntry = (entry: Entry) => { if (entry.released) return; entry.released = true; entry.closeRelay?.(); entry.releaseTicket?.() }
const release = releasePreviewEntry
export function setPreview(id: string, patch: Partial<PreviewState>) {
  // The mini player floats the embedded page; without a web view there is nothing to float.
  if (patch.floating && !webHost.available) patch = { ...patch, floating: false }
  if (patch.floating) for (const [other, state] of sessions) if (other !== id && state.floating) sessions.set(other, { ...state, floating: false })
  if (!sessions.has(id) && sessions.size >= 40) { const key = sessions.keys().next().value!; sessions.get(key)?.entries.forEach(release); sessions.delete(key); generations.delete(key) }
  sessions.set(id, { ...previewState(id), ...patch }); listeners.forEach(l => l())
}
/** The browser of a thread is a tab beside its chat (ADE-54); a thread off screen remembers it for when it is shown. `background`
 *  (an agent's request) adds the tab without switching to it, so the chat and the composer keep the reader's place and focus. */
export function showPreview(id: string, background = false) { openTab(id, { kind: 'preview' }, background ? { activate: false, focus: false } : {}) }
function commitEntry(id: string, entry: Entry, replace: boolean, load: PreviewLoad) {
  const current = previewState(id)
  const replacing = replace && !!current.entries[current.index]
  let entries: Entry[]
  if (replacing) {
    release(current.entries[current.index]!)
    entries = current.entries.map((old, index) => index === current.index ? entry : old)
  } else {
    if (current.entries[current.index]) release(current.entries[current.index]!)
    current.entries.slice(current.index + 1).forEach(release)
    entries = [...current.entries.slice(0, current.index + 1), entry]
    if (entries.length > 16) entries.splice(0, entries.length - 16).forEach(release)
  }
  setPreview(id, { entries, index: replacing ? current.index : entries.length - 1, floating: entry.kind === 'external' ? false : current.floating, busy: false, loading: load.phase === 'ready' && webHost.available, load, wsFailed: false, wsNoticeDismissed: false, navigatedAway: false, opened: false, bridge: undefined, pageTitle: undefined, pagePath: undefined })
}
const REMOTE_FILE_MESSAGE = "Files on another machine can't be previewed here yet. Ask the agent to serve the page, then open its address."
export interface OpenOptions { proxy?: boolean; retry?: boolean; suffix?: string; /** A server on another machine; its port is forwarded to this machine first. */ machine?: RemoteServer; /** The forward exists: `raw` is its local address. */ forwarded?: boolean
  /** The forward's Fast mode; `raw` is then its `https://localhost` address and `fast.plain` the one to show and copy. */ fast?: FastMode; trust?: { sha256: string; pem: string }
  /** Open through the thread machine's own preview ticket, not a forward (the forward could not be opened). */ ticketRoute?: boolean
  /** Who asks for the forward: "Forward a port…" opens a `manual` one that stays; every other open asks for a `preview` one,
   *  which ends with the browser's last tab (ADE-128). */ origin?: PortForwardOrigin }
/** A server of a thread that runs on another machine: forwarded to this machine over the direct route when the machine is in
 * this machine's network (keep-alive TCP, no relay hop), else `undefined` and the page goes through that machine's ticket. */
function threadServer(id: string, raw: string, options: OpenOptions): RemoteServer | undefined {
  if (options.machine || options.forwarded || options.ticketRoute || !activeEnvironment().local) return undefined
  const machineId = machineOfThreadId(id)
  const row = machineId ? machineRows(machineHub()).find(m => m.machineId === machineId) : undefined
  return row ? localServerOn(raw, { id: row.machineId, name: shortMachineName(row.name) || row.name, offline: row.offline }) : undefined
}
export async function openPreview(id: string, raw: string, allowFolder = false, launch = false, replace = false, options: OpenOptions = {}): Promise<boolean | undefined> {
  const earlier = previewState(id), machine = options.machine ?? (replace && !options.ticketRoute ? earlier.entries[earlier.index]?.machine : undefined)
  if (machine && !options.forwarded) return openForwarded(id, machine, launch, replace, options)
  const served = threadServer(id, raw, options)
  if (served) return openForwarded(id, served, launch, replace, { ...options, fallback: raw })
  const via = options.forwarded ? options.machine : undefined
  const thread = getState().threads.find(t => t.id === id)
  const project = getState().projects.find(p => p.id === thread?.project_id)
  const target = classifyPreviewInput(raw, { projectRoot: thread?.cwd ?? project?.path, projectName: project?.name })
  if (target.kind === 'empty') return
  if (target.kind === 'rejected') { setPreview(id, { error: PREVIEW_REJECTED_MESSAGE }); return false }
  const address = target.kind === 'search' ? `https://www.google.com/search?q=${encodeURIComponent(target.query)}` : target.kind === 'file' ? target.path : target.url
  const generation = ++nextGeneration; generations.set(id, generation)
  const before = previewState(id), old = before.entries[before.index]
  // A thread on another machine of the network: its servers are proxied by that machine and reached through its connection.
  const remote = !via && !!machineOfThreadId(id), near = activeEnvironment().local && !remote
  const proxy = !near || (options.proxy ?? (replace && old?.kind === 'server' ? old.proxy : false))
  setPreview(id, { busy: !options.retry, attemptedAddress: address, attemptedMachine: machine, loading: !options.retry, error: null, permission: undefined, load: options.retry && before.load.phase === 'waiting' ? before.load : { phase: 'resolving' } })
  let pendingEntry: Entry | undefined
  try {
    // Forwarded pages load immediately; the web view reports connection and navigation failures.
    const client = (via ? localClient() : threadClient(id))!
    if (target.kind === 'server' && proxy && new URL(address).protocol === 'https:') {
      throw Object.assign(new Error("HTTPS servers cannot be proxied yet. Use the server's HTTP port, or open its HTTPS address directly on that machine."), { data: { code: 'unsupported_proxy' } })
    }
    let load: PreviewLoad = { phase: target.kind === 'external' || target.kind === 'search' ? 'external' : 'ready' }
    let title: string | undefined
    if (target.kind === 'server' && !via && !(proxy && near)) {
      const probe: PreviewProbeResult = await client.call('previews.probe', { url: address })
      if (generations.get(id) !== generation) return false
      title = probe.title
      if (!probe.reachable) {
        const error = probe.error
        load = !error || error === 'connection_refused' || error === 'timed_out'
          ? { phase: 'waiting', error: error === 'timed_out' ? error : 'connection_refused', startedAt: before.load.phase === 'waiting' && options.retry ? before.load.startedAt : Date.now() }
          : { phase: 'error', error, status: probe.status }
      } else if (probe.blocked_by && !proxy) load = { phase: 'blocked', ...probe.blocked_by }
    }
    const result: PreviewOpenResult = load.phase === 'ready' || load.phase === 'external'
      ? via ? await client.call('previews.server.open', { url: address, proxy })
        : await client.call('previews.open', { thread_id: id, target: address, proxy, ...(allowFolder ? { allow_folder: true } : {}) })
      : { target: { kind: 'server', url: address, port: Number(new URL(address).port) || 80 } }
    if (generations.get(id) !== generation) { if (result.ticket) await client.call('previews.close', { ticket: result.ticket }); return }
    if (result.needs_permission) { setPreview(id, { busy: false, loading: false, load: { phase: 'idle' }, permission: { address, ...result.needs_permission } }); return false }
    if (remote && result.target.kind === 'file') { if (result.ticket) void client.call('previews.close', { ticket: result.ticket }).catch(() => {}); throw new Error(REMOTE_FILE_MESSAGE) }
    // Another machine's ticket has no address here: its relay below is the page's address.
    const url = result.path ? remote ? result.path : `${httpBase(client.endpoint.url)}${result.path}${result.target.kind === 'file' ? options.suffix ?? '' : ''}` : result.target.kind === 'file' ? null : result.target.url
    if (!url) throw new Error('Unable to open this local file. Choose an HTML file and try again.')
    // A Fast page loads its `https://localhost` listener; the address shown, copied and opened in a browser is the plain one.
    const shown = options.fast?.on ? options.fast.plain : address
    const entry: Entry = { target: result.target, proxy: !!proxy, ...(target.kind === 'search' ? { query: target.query } : {}), address: result.target.kind === 'file' ? result.target.path : shown, url, title: title ?? (result.target.kind === 'file' ? result.target.path.split('/').pop() ?? 'Preview' : new URL(shown).host), ticket: result.ticket, kind: result.target.kind, ...(via ? { machine: via } : {}), ...(via && options.fast ? { fast: options.fast } : {}), ...(via && options.trust ? { trust: options.trust } : {}) }
    pendingEntry = entry
    entry.releaseTicket = () => { if (result.ticket) void client.call('previews.close', { ticket: result.ticket }).catch(() => {}) }
    if (result.target.kind === 'server' && proxy && !result.path) throw Error('Kybern cannot forward this server through the current connection.')
    if (result.target.kind === 'server' && result.path) {
      const onWebSocketFailure = () => { if (generations.get(id) === generation) setPreview(id, { wsFailed: true }) }
      const relay = remote ? await client.openPage(result.path, { onWebSocketFailure }) : await openRelay(url, httpBase(client.endpoint.url), { onWebSocketFailure })
      if (remote) entry.url = relay.url
      const original = new URL(address)
      entry.frameUrl = `${relay.url.replace(/\/$/, '')}${original.pathname}${original.search}${original.hash}`; entry.closeRelay = relay.close
      if (generations.get(id) !== generation) { release(entry); return }
    }
    commitEntry(id, entry, replace, load)
    pendingEntry = undefined
    // Persist the source, never the ephemeral local port allocated by ports.forward.
    let recentSource = via
    if (!recentSource && entry.kind === 'server' && client.info) {
      const url = new URL(entry.address)
      if (['localhost', '127.0.0.1', '[::1]'].includes(url.hostname)) recentSource = { id: client.info.machine_id ?? client.info.environment_id, name: client.info.hostname, port: Number(url.port) || (url.protocol === 'https:' ? 443 : 80), path: addressPath(entry.address), scheme: url.protocol === 'https:' ? 'https' : 'http' }
    }
    if (entry.kind !== 'external') recordRecent(recentsKeyForThread(id), { kind: entry.kind, value: recentSource ? sourceAddress(recentSource) : entry.address, title: entry.title, at: Date.now(), ...(recentSource ? { machine: recentSource } : {}) })
    // No embedded web view: the page opens in the default browser as soon as it is ready (the entry keeps its ticket and relay until replaced or closed).
    if (launch || (!webHost.available && entry.kind !== 'external' && load.phase === 'ready')) await launchPreview(id)
    return true
  } catch (e) {
    if (pendingEntry) release(pendingEntry)
    if (generations.get(id) !== generation) return false
    const unsupported = !!via && !!e && typeof e === 'object' && 'code' in e && e.code === -32601
    const unsupportedProxy = !!e && typeof e === 'object' && 'data' in e && !!e.data && typeof e.data === 'object' && 'code' in e.data && e.data.code === 'unsupported_proxy'
    const message = unsupported
      ? `Update Kybern on ${activeEnvironment().name} to open servers from your machines.`
      : e instanceof Error ? e.message : String(e)
    if (target.kind === 'server') {
      const entry: Entry = { address, url: address, kind: 'server', proxy: !!proxy, title: new URL(address).host, released: true, ...(via ? { machine: via } : {}) }
      commitEntry(id, entry, replace, proxy && !unsupported && !unsupportedProxy ? { phase: 'relay-failed' } : { phase: 'error', message })
    } else if (replace && old?.kind === 'file' && /doesn't exist|does not exist|not.found|isn't there/i.test(message)) {
      commitEntry(id, { ...old, released: true }, true, { phase: 'not-found', name: address.split('/').pop() ?? address })
    } else setPreview(id, { busy: false, loading: false, error: message, load: { phase: 'error', message } })
    return false
  }
}
/** Forward the port of a server on another machine, then open the forwarded address like any local page. Failures show in the pane.
 * Above `FAST_RTT_MS` (or when chosen by hand) the page loads over the forward's HTTP/2 listener (Fast mode, `fast.ts`). */
async function openForwarded(id: string, machine: RemoteServer, launch: boolean, replace: boolean, options: OpenOptions & { fallback?: string }): Promise<boolean | undefined> {
  const generation = ++nextGeneration; generations.set(id, generation)
  const before = previewState(id)
  setPreview(id, { busy: !options.retry, loading: !options.retry, attemptedMachine: machine, attemptedAddress: sourceAddress(machine), error: null, permission: undefined, load: options.retry && before.load.phase === 'waiting' ? before.load : { phase: 'resolving' } })
  try {
    const client = localClient()!
    const primaryId = client.info?.machine_id ?? client.info?.environment_id
    let address = sourceAddress(machine), fast: FastMode | undefined, trust: Entry['trust']
    if (primaryId !== machine.id) {
      const origin = options.origin ?? 'preview'
      const forward = await forwardPort(client, machine.id, machine.port, origin)
      const plain = forwardedAddress(forward, machine.path, machine.scheme)
      address = plain
      if (webHost.available && machine.scheme !== 'https' && forward.https_available) {
        fast = { on: false, plain, rttMs: forward.rtt_ms }
        if (wantsFast(forward.rtt_ms, previewState(id).fastChoice?.[fastKey(machine)])) {
          const secure = await client.call('ports.forward', { machine: machine.id, remote_port: machine.port, https: true, origin }).catch(() => undefined)
          const fastUrl = secure && fastAddress(secure, machine.path)
          if (fastUrl && secure.https_certificate && secure.https_certificate_sha256) {
            address = fastUrl; fast = { ...fast, on: true }; trust = { sha256: secure.https_certificate_sha256, pem: secure.https_certificate }
          }
        }
      }
    }
    if (generations.get(id) !== generation) return false
    return await openPreview(id, address, false, launch, replace, { ...options, machine, forwarded: true, fast, trust })
  } catch (e) {
    if (generations.get(id) !== generation) return false
    // A thread's own server whose machine could not forward it (not in this machine's network): its preview ticket instead.
    if (options.fallback) return openPreview(id, options.fallback, false, launch, replace, { ...options, machine: undefined, ticketRoute: true })
    const message = e instanceof Error ? e.message : String(e)
    const address = sourceAddress(machine)
    commitEntry(id, { address, url: address, title: machine.name, kind: 'server', machine, released: true }, replace, { phase: 'error', message })
    setPreview(id, { error: message, loading: false })
    return false
  }
}
/** Show a dev server of another machine in the Browser. */
export const openRemoteServer = (id: string, machine: RemoteServer, launch = false, origin?: PortForwardOrigin) => openPreview(id, `${machine.name}:${machine.port}`, false, launch, false, { machine, ...(origin ? { origin } : {}) })
export async function launchPreview(id: string) {
  const entry = previewState(id).entries[previewState(id).index]
  if (!entry) return
  const current = () => previewState(id).entries[previewState(id).index] === entry
  try {
    // A forwarded server is reached from this machine; another machine's own server only through its relay.
    const local = activeEnvironment().local && (!!entry.machine || !machineOfThreadId(id))
    if (entry.kind === 'file' && !local) return
    await launchBrowser(browserAddress(entry, previewState(id).pagePath, local, webHost.available)); if (current()) setPreview(id, { opened: true, error: null }) }
  catch (error) { if (current()) setPreview(id, { error: String(error) }) }
}
/** What the browser-only state copies: the page's address (never a ticket URL, which works once). */
export function previewLink(id: string): string | null {
  const s = previewState(id), entry = s.entries[s.index]
  return entry ? currentAddress(entry, s.pagePath).full : null
}
export function navigatePreview(id: string, direction: number) {
  const current = previewState(id), index = current.index + direction
  const entry = current.entries[index]
  if (!entry) return
  generations.set(id, ++nextGeneration)
  release(current.entries[current.index]!)
  setPreview(id, { index, busy: false, opened: false, error: null, attemptedAddress: undefined, attemptedMachine: undefined, permission: undefined, bridge: undefined, pageTitle: undefined, pagePath: undefined })
  void openPreview(id, entry.address, false, false, true)
}
export function closePreview(id: string) {
  generations.set(id, ++nextGeneration)
  previewState(id).entries.forEach(release)
  setPreview(id, { entries: [], load: { phase: 'idle' }, loading: false, index: -1, floating: false, opened: false, busy: false, error: null, attemptedAddress: undefined, attemptedMachine: undefined, permission: undefined, bridge: undefined, pagePath: undefined, pageTitle: undefined })
}

/** The thread's last browser tab closed (ADE-128): its page, history, relays, tickets and choices (Fast, device, servers
 *  machine) go, and late responses are dropped, so the next browser tab starts fresh at the servers list. Its web view goes
 *  with the page (`PreviewSurface`). */
export function disposePreview(id: string) {
  const state = sessions.get(id)
  if (!state) return
  generations.set(id, ++nextGeneration)
  state.entries.forEach(release)
  sessions.delete(id)
  listeners.forEach(listener => listener())
}

/** Environment lifecycle hook: clear metadata and invalidate all late responses. */
export function resetPreviews() {
  for (const session of sessions.values()) session.entries.forEach(release)
  sessions.clear(); generations.clear(); nextGeneration++
  listeners.forEach(listener => listener())
}

export function retryPreview(id: string) {
  const s = previewState(id), entry = s.entries[s.index]
  if (entry || s.attemptedAddress) return openPreview(id, s.attemptedAddress ?? entry!.address, false, false, true, { retry: true, ...(s.attemptedMachine ? { machine: s.attemptedMachine } : {}) })
}
/** Turn Fast mode on or off for the current page's server (remembered for that server in this Browser), and reload it. */
export function setPreviewFast(id: string, on: boolean) {
  const s = previewState(id), entry = s.entries[s.index]
  if (entry?.kind !== 'server' || !entry.machine) return
  setPreview(id, { fastChoice: { ...s.fastChoice, [fastKey(entry.machine)]: on } })
  void openPreview(id, entry.address, false, false, true, { machine: entry.machine })
}
export function setPreviewProxy(id: string, proxy: boolean) {
  const s = previewState(id), entry = s.entries[s.index]
  if (entry?.kind === 'server') void openPreview(id, entry.address, false, false, true, { proxy })
}
