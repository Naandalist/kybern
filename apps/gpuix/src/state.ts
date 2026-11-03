import { activeEnvironment, environmentEndpoint } from './shell/environment.ts'
import { machineHub } from './machines/index.ts'
import { ListMerger, byPinnedThenNewest, type StateThread } from './machines/merged.ts'
import { scopedKey } from '../../../packages/kybern-client/src/machineMerge.ts'
import { createGpuixSocket } from './machines/socket.ts'
import { startupMark } from './perf/startup.ts'
import { addMemoryContext, memoryStep } from './perf/memoryLog.ts'
import { HistoryPrefetch } from './shell/historyPrefetch.ts'
import { EARLIER_HISTORY_PAGE } from '../../../packages/kybern-client/src/historyPaging.ts'
import { onFrame } from './native/frame.ts'
import { isWindowVisible, onWindowVisibility } from './native/visibility.ts'
import { eventFrame } from './transcript/eventFrame.ts'
// App state: one daemon connection, the project and thread lists, and the open
// thread's transcript. Transport and transcript folding come from the shared
// `packages/kybern-client` sources, the same ones desktop and mobile use.
import { createContext, createElement, useContext, useMemo, useSyncExternalStore, type ReactNode } from 'react'
import { KybernClient, type ConnectionStatus } from '../../../packages/kybern-client/src/client.ts'
import {
  applyEvent,
  compactThreadState,
  emptyThreadState,
  seedFromGet,
  type ThreadState,
} from '../../../packages/kybern-client/src/transcript.ts'
import { isChildThread, isInboundThread } from '../../../packages/kybern-client/src/subagents.ts'
import type { MachineId, MethodName, ParamsOf, Project, ResultOf, Thread, ThreadActivitySummary, ThreadEvent, ThreadId } from '../../../packages/kybern-client/src/types.ts'
import { mergeIntoCurrent } from '../../../packages/kybern-client/src/newChat.ts'
import { newChats } from './composer/newChats.ts'
import { getNav, setNav } from './nav/store.ts'
import { OriginChildren, remoteChildStore, underParents, withRemoteChildren } from './nav/remoteChildren.ts'
import { RemoteRecordOverlay, remoteDelegationStore } from './nav/remoteDelegations.ts'

export interface AppState {
  status: ConnectionStatus
  statusDetail: string | null
  projects: Project[]
  /** The machine the user is at lists as it always did; threads of other machines carry `machineId` (`machines/merged.ts`). */
  threads: StateThread[]
  /** `null` shows the home (Draft) screen. */
  selectedId: ThreadId | null
  /** The machine that owns the selected thread; unset for the one the user is at. */
  selectedMachineId?: MachineId
  /**
   * What the home screen starts while no thread is selected (desktop
   * `selected.kind === "draft"`): a project's new thread, its coordinator, or
   * a free chat when `projectId` is unset.
   */
  draft: { projectId?: string; purpose?: 'thread' | 'coordinator' }
  open: ThreadState
  collapsed: Record<string, boolean>
  sidebarOpen: boolean
  /** The project and thread lists have arrived at least once (the Welcome screen waits for them). */
  listsLoaded: boolean
}

let state: AppState = {
  status: 'idle',
  statusDetail: null,
  projects: [],
  threads: [],
  selectedId: null,
  draft: {},
  open: emptyThreadState(),
  collapsed: {},
  sidebarOpen: true,
  listsLoaded: false,
}
const listeners = new Set<() => void>()

export interface PaneState {
  selectedId: ThreadId | null
  /** The machine that owns `selectedId`; unset for the machine the user is at. */
  machineId?: MachineId
  open: ThreadState
  draft: AppState['draft']
  generation: number
}
export const DEFAULT_PANE = 'main'
let focusedPaneId = DEFAULT_PANE
const panes = new Map<string, PaneState>([[DEFAULT_PANE, { selectedId: null, open: state.open, draft: {}, generation: 0 }]])
const PaneContext = createContext<{ id: string; snapshot?: AppState; compact?: boolean } | null>(null)
export function PaneProvider({ id, snapshot, compact, children }: { id: string; snapshot?: AppState; compact?: boolean; children: ReactNode }) {
  const inherited = useContext(PaneContext)?.compact
  const small = compact ?? inherited
  const value = useMemo(() => ({ id, snapshot, compact: small }), [id, snapshot, small])
  return createElement(PaneContext.Provider, { value }, children)
}
export function usePaneCompact() { return !!useContext(PaneContext)?.compact }
export function usePaneId() { return useContext(PaneContext)?.id ?? focusedPaneId }
export function useFrozenPane() { return !!useContext(PaneContext)?.snapshot }
export const getFocusedPaneId = () => focusedPaneId
export const getPane = (id: string) => panes.get(id)
export const getPaneIds = () => [...panes.keys()]
export function getPaneState(id: string): AppState {
  const pane = panes.get(id)
  return pane ? { ...state, ...pane } : { ...state, selectedId: null, open: emptyThreadState(), draft: {} }
}
/** Capture the pane at render time; async callbacks never retarget on focus changes. */
export function usePaneAccess() {
  const context = useContext(PaneContext)
  const id = context?.id ?? focusedPaneId
  return useMemo(() => ({ id, get: () => context?.snapshot ?? getPaneState(id), frozen: !!context?.snapshot }), [id, context])
}
const publication = eventFrame(flush => {
  const stop = onFrame(flush)
  const stopVisibility = onWindowVisibility(visible => { if (!visible) flush() })
  return () => { stop(); stopVisibility() }
}, () => { for (const listener of listeners) listener() })
function emit() { publication.now() }
function mirrorFocus() {
  const pane = panes.get(focusedPaneId)!
  state = { ...state, selectedId: pane.selectedId, selectedMachineId: pane.machineId, open: pane.open, draft: pane.draft }
}
export function createPane(id: string) {
  if (!panes.has(id)) panes.set(id, { selectedId: null, open: emptyThreadState(), draft: {}, generation: 0 })
}
/** Panes hosted inside another (a pinned child-thread tab) focus their host instead (`tabs/open.ts`). */
let paneAlias: (id: string) => string = (id) => id
export function setPaneAlias(alias: (id: string) => string) { paneAlias = alias }
export function focusPane(requested: string) {
  const id = paneAlias(requested)
  if (!panes.has(id) || focusedPaneId === id) return
  focusedPaneId = id
  mirrorFocus(); emit()
}
export function setPane(id: string, patch: Partial<PaneState>) {
  const pane = panes.get(id)
  if (!pane) return
  const next = { ...pane, ...patch }
  panes.set(id, next)
  if (patch.open && next.selectedId) {
    const session = sessions.get(paneSessionKey(next))
    if (session) session.open = patch.open
    for (const [otherId, other] of panes) if (otherId !== id && samePaneThread(other, next)) panes.set(otherId, { ...other, open: patch.open })
  }
  mirrorFocus(); emit()
}
/** Legacy callers continue addressing the focused pane. */
export function set(patch: Partial<AppState>) {
  state = { ...state, ...patch }
  setPane(focusedPaneId, {
    // A new selection names its machine or means the one the user is at.
    ...(patch.selectedId !== undefined ? { selectedId: patch.selectedId, machineId: patch.selectedMachineId } : {}),
    ...(patch.open !== undefined ? { open: patch.open } : {}),
    ...(patch.draft !== undefined ? { draft: patch.draft } : {}),
  })
}
export function useApp<T>(select: (s: AppState) => T): T {
  const context = useContext(PaneContext)
  return useSyncExternalStore(subscribe, () => select(context?.snapshot ?? (context ? getPaneState(context.id) : state)))
}
export const usePane = useApp
export const getState = () => state

/** Observe every state change (navigation history, persistence). */
export function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => listeners.delete(listener)
}


let client: KybernClient | null = null
/**
 * The daemon connection of the machine the user is at, once `connect()` has run. Machine-wide surfaces of this
 * machine (its settings, accounts, spaces, usage) use it. Anything that belongs to a thread or a project asks
 * `threadClient` / `projectClient` for the machine that owns it; `shell/machines.lint.test.ts` keeps the
 * thread-scoped folders honest.
 */
export const getClient = () => client
/**
 * The same connection, named for what it is: surfaces that are about the machine the user is at, not about a thread or
 * a project (its settings, accounts, providers, usage). Thread-scoped folders may use this, never `getClient()`.
 */
export const localClient = getClient
export { clientFor, clientForProject, clientForThread, useMachineView, useMachines } from './machines/index.ts'
export type { StateThread } from './machines/merged.ts'

// ---- which machine owns what ----

/** The machine that owns a thread, or `undefined` for the one the user is at (the common case, answered without a lookup). */
export function machineOfThreadId(id: ThreadId, hint?: MachineId): MachineId | undefined {
  const hub = machineHub()
  if (hint) return hint === hub.localMachineId ? undefined : hint
  if (hub.machineCount <= 1) return undefined
  const listed = state.threads.find(t => t.id === id)
  if (listed) return listed.machineId
  // A subagent or delegated child is reached from its parent, never listed: `nav.children` knows its machine.
  const child = getNav().children[id] as StateThread | undefined
  if (child) return child.machineId
  const owner = hub.machineOfThread(id)
  return owner === hub.localMachineId ? undefined : owner
}
/** The machine that owns a project: the listed project's own machine for one only another machine has, else the local one. */
export function machineOfProjectId(id: string, hint?: MachineId): MachineId | undefined {
  const hub = machineHub()
  if (hint) return hint === hub.localMachineId ? undefined : hint
  if (hub.machineCount <= 1) return undefined
  const owner = hub.set.machineOfProject(id)
  return owner === hub.localMachineId ? undefined : owner
}
/**
 * The client of the machine that owns a thread: the app's own connection for the machine the user is at, the
 * machine set's for another. Throws `OfflineError` while another machine cannot be reached. `null` before the first connect.
 * Pass the thread's `machineId` when the caller has it (ids are unique per machine only).
 */
export function threadClient(id: ThreadId | null | undefined, machineId?: MachineId): KybernClient | null {
  const machine = id ? machineOfThreadId(id, machineId) : machineId ? machineOfThreadId('', machineId) : undefined
  return machine ? machineHub().clientFor(machine) : client
}
/** `threadClient` for a project (`machineId` picks one of several checkouts). */
export function projectClient(id: string | null | undefined, machineId?: MachineId): KybernClient | null {
  const machine = id ? machineOfProjectId(id, machineId) : machineId ? machineOfProjectId('', machineId) : undefined
  return machine ? machineHub().clientFor(machine) : client
}
/** One call on the machine that owns a thread. Rejects (never throws) when that machine is offline or the app is not connected. */
export async function callThread<M extends MethodName>(id: ThreadId | null | undefined, method: M, params: ParamsOf<M>, machineId?: MachineId): Promise<ResultOf<M>> {
  const owner = threadClient(id, machineId)
  if (!owner) throw new Error('Not connected')
  return owner.call(method, params)
}
/** A thread's own project id on its machine (listed threads of other machines were moved onto the merged project row). */
export const ownProjectId = (thread: { project_id: string; ownProjectId?: string }) => thread.ownProjectId ?? thread.project_id

type Session = { threadId: ThreadId; machineId?: MachineId; open: ThreadState; history: HistoryPrefetch; historyTimer?: ReturnType<typeof setTimeout>; stop?: () => void; ready?: Promise<void> }
/** Open transcripts, keyed by `(machine, thread)`: ids are unique per machine, not across machines. The machine the user is at is `local`. */
const sessions = new Map<string, Session>()
const LOCAL_KEY = 'local'
const sessionKey = (id: ThreadId, machineId?: MachineId) => scopedKey(machineId ?? LOCAL_KEY, id)
const paneSessionKey = (pane: Pick<PaneState, 'selectedId' | 'machineId'>) => sessionKey(pane.selectedId!, pane.machineId)
const samePaneThread = (a: Pick<PaneState, 'selectedId' | 'machineId'>, b: Pick<PaneState, 'selectedId' | 'machineId'>) => a.selectedId === b.selectedId && a.machineId === b.machineId
/** The session of a thread; by id alone when the caller does not know the machine (it resolves like `threadClient`). */
function sessionOf(id: ThreadId, machineId?: MachineId): Session | undefined {
  return sessions.get(sessionKey(id, machineId ?? machineOfThreadId(id)))
}
/** Prepare one immutable older page after the newest content has painted.
 * No transcript rows are mounted until the reader reaches its existing gate. */
export function primeThreadHistory(id: ThreadId) {
  if (process.env.KYBERN_HISTORY_PREFETCH === '0') return
  const session = sessionOf(id), owner = sessionClient(session)
  if (!session?.open.loaded || session.open.nextBeforeSeq == null || session.open.loadingEarlier || !owner) return
  clearTimeout(session.historyTimer)
  session.historyTimer = setTimeout(() => {
    session.historyTimer = undefined
    if (owner !== sessionClient(session) || sessionOf(id, session.machineId) !== session) return
    session.history.prime(session.open, (cursor, through) => owner.call('threads.get', {
      thread_id: id, include_tool_output: false, defer_tool_stream: true,
      ...EARLIER_HISTORY_PAGE, before_seq: cursor, through_seq: through,
    }))
  }, 250)
}
export const takeThreadHistory = (id: ThreadId, base: ThreadState) => sessionOf(id)?.history.take(base)
/** The client a session reads and subscribes through: the app's own for the machine the user is at, else that machine's. */
function sessionClient(session: Session | undefined): KybernClient | null {
  if (!session) return null
  if (!session.machineId) return client
  try { return machineHub().clientFor(session.machineId) } catch { return null }
}
function releaseUnused() {
  for (const [key, session] of sessions) if (![...panes.values()].some(p => p.selectedId && paneSessionKey(p) === key)) {
    sessions.delete(key); session.stop?.()
  }
}
export function closePaneState(id: string) {
  if (panes.size <= 1 || !panes.has(id)) return
  panes.delete(id); releaseUnused()
  if (focusedPaneId === id) focusedPaneId = panes.keys().next().value!
  mirrorFocus(); emit()
}
/** Identity token for asynchronous paging/tool hydration across close/reopen. */
export const getThreadSession = (id: string) => sessionOf(id)
export async function reloadPaneThread(paneId: string, threadId: string) {
  const machineId = panes.get(paneId)?.machineId
  sessionOf(threadId, machineId)?.stop?.(); sessions.delete(sessionKey(threadId, machineId ?? machineOfThreadId(threadId)))
  return selectPaneThread(paneId, threadId, false, machineId)
}
/** Characters of text in a transcript, visiting at most `budget` values (the memory log's size estimate). */
function textChars(value: unknown, budget = { left: 200_000 }): number {
  if (typeof value === 'string') return value.length
  if (!value || typeof value !== 'object' || --budget.left < 0) return 0
  let chars = 0
  for (const item of Array.isArray(value) ? value : Object.values(value)) chars += textChars(item, budget)
  return chars
}
addMemoryContext('panes', () => [...panes].map(([id, pane]) => ({
  id, thread: pane.selectedId, loaded: pane.open.loaded, blocks: pane.open.blocks.length,
  moreHistory: pane.open.nextBeforeSeq != null, textKChars: Math.round(textChars(pane.open.blocks) / 1000),
})))
addMemoryContext('sessions', () => sessions.size)
export function getThreadOpen(id: string) { return sessionOf(id)?.open ?? [...panes.values()].find(p => p.selectedId === id)?.open }
export function setThreadOpen(id: string, open: ThreadState, textOnly = false, machineId?: MachineId) {
  const session = sessionOf(id, machineId)
  if (session) session.open = open
  const owner = machineId ?? machineOfThreadId(id)
  for (const [paneId, pane] of panes) if (pane.selectedId === id && pane.machineId === owner) panes.set(paneId, { ...pane, open })
  mirrorFocus()
  if (textOnly && isWindowVisible() && process.env.KYBERN_STREAM_EVENT_FRAME !== '0') publication.next()
  else emit()
}

/** Drop a dead client so `connect()` builds a fresh one, re-reading the port and token files. */
let connectionAttempt = 0
export function resetClient() {
  connectionAttempt++
  for (const session of sessions.values()) session.stop?.()
  sessions.clear()
  for (const [id, pane] of panes) panes.set(id, { ...pane, open: emptyThreadState(), generation: pane.generation + 1 })
  mirrorFocus(); emit()
  const previous = client
  client = null
  machineHub().releasePrimary()
  clearTimeout(retryTimer); refreshing = null; refreshAgain = false
  lastActivity = undefined
  previous?.close()
}

export async function connect() {
  if (client) return
  const attempt = ++connectionAttempt
  startupMark('endpoint-start')
  let ep
  try {
    const endpoint = environmentEndpoint()
    ep = endpoint instanceof Promise ? await endpoint : endpoint
    if (attempt !== connectionAttempt) return
    startupMark('endpoint-ready')
  } catch (error) {
    if (attempt !== connectionAttempt) return
    set({ status: 'failed', statusDetail: error instanceof Error ? error.message : 'Start kybernd, then reconnect' })
    return
  }
  // Native sockets authenticate the upgrade with a header and use the trusted
  // app origin, like the mobile client's `createNativeSocket`.
  client = new KybernClient(ep, { createSocket: createGpuixSocket })
  const owner = client
  machineHub().beginPrimary(owner, { id: activeEnvironment().id, name: activeEnvironment().name })
  client.onStatus((status, detail) => {
    if (owner !== client) return
    set({ status, statusDetail: detail ?? null })
    startupMark('connection', { status })
    if (status === 'open') {
      // Queue the inexpensive navigation index first, then hydrate saved chats
      // concurrently. Git probes refresh after the index is already usable.
      void refreshLists()
      reloadOpenTranscripts()
      // Joins the machine set under the daemon's id; a failure there never touches this connection.
      void machineHub().primaryOpened(owner).then(() => watchMachines()).catch(() => {})
    }
  })
  client.onNotification('projects.changed', () => void refreshLists())
  client.onNotification('threads.changed', () => void refreshLists())
  client.connect()
}

let refreshing: Promise<void> | null = null
let refreshAgain = false
let retryTimer: ReturnType<typeof setTimeout> | undefined
const activityListeners = new Set<(activity: ThreadActivitySummary[]) => void>()
let lastActivity: { owner: KybernClient; activity: ThreadActivitySummary[] } | undefined
/** Reuse the list read's activity projection instead of issuing another boot RPC. */
export function onThreadActivity(listener: (activity: ThreadActivitySummary[]) => void) {
  activityListeners.add(listener)
  if (lastActivity?.owner === client) listener(lastActivity.activity)
  return () => { activityListeners.delete(listener) }
}

/**
 * Reload the project and thread lists. Calls that arrive while one is in flight
 * (a burst of `threads.changed` from busy agents) fold into a single rerun, so a
 * slow daemon gets at most one list request at a time. A failed first load
 * retries after 3 s instead of leaving the window on "Connecting".
 */
export function refreshLists(): Promise<void> {
  if (refreshing) {
    refreshAgain = true
    return refreshing
  }
  const owner = client
  refreshing = (async () => {
    try {
      do {
        refreshAgain = false
        await loadLists()
      } while (refreshAgain && owner === client)
    } catch {
      if (owner !== client) return
      clearTimeout(retryTimer)
      if (!state.listsLoaded && client?.status === 'open') retryTimer = setTimeout(() => void refreshLists(), 3000)
    } finally {
      if (owner !== client) return
      refreshing = null
      // A request that arrived while a failing load was in flight (say, from a
      // reconnect) still runs once the connection is open.
      if (refreshAgain && client?.status === 'open') {
        refreshAgain = false
        void refreshLists()
      }
    }
  })()
  return refreshing
}

/** The daemon's last thread list; new chats it does not list yet are shown with it (`composer/newChats.ts`). */
let listedThreads: Thread[] = []
/** Every optimistic thread object put in the list, so a merge can take them out again without touching the daemon's. */
const optimisticThreads = new WeakSet<Thread>()
/** The list with the new chats merged in. Without `base` it is the current list (live events and local changes included),
 *  never the last `loadLists()` snapshot, which would bring back archived threads and old statuses and titles. */
function withNewChats(base: readonly Thread[] = state.threads): Thread[] {
  const { threads, listed } = mergeIntoCurrent(base, newChats.unlisted(), optimisticThreads)
  if (listed.length) queueMicrotask(() => newChats.listed(listed))
  return threads
}
// A new chat appears in the list (and its page) on the frame it starts, follows its create's state while the daemon has no
// row for it, and loads its transcript once the daemon made it. The server copy keeps the id, so no row remounts.
newChats.subscribe(() => {
  set({ threads: withNewChats() })
  for (const [paneId, pane] of panes) {
    const id = pane.selectedId
    const chat = newChats.get(id)
    if (!id || sessions.has(sessionKey(id)) || chat?.adoptedId) continue
    // Created, or listed by the daemon after a create that failed here (a dropped reply): the daemon's thread loads.
    const loads = chat ? chat.state === 'created' : !!pane.open.thread && optimisticThreads.has(pane.open.thread)
    if (loads) void selectPaneThread(paneId, id, true, chat?.machineId).catch(error => console.error('[pane] Unable to load thread:', error))
    else if (chat && pane.open.thread?.status !== chat.thread.status) setPane(paneId, { open: { ...pane.open, thread: chat.thread } })
  }
})

// ---- the merged lists (design §5.2 step 2) ----

const merger = new ListMerger()
/** Project ids that only another machine has: listed after the local projects. */
let foreignIds = new Set<string>()
/** Machines that have a checkout of a listed project, for the project row's tooltip. */
let machinesOf = new Map<string, MachineId[]>()
/** The machines that have a checkout of a listed project (empty with one machine). */
export const machinesOfProject = (projectId: string): readonly MachineId[] => machinesOf.get(projectId) ?? []
/** The space another machine's projects are listed in; `spaces/` registers it (the default space). */
let foreignSpace: () => string | null = () => null
export function setForeignSpace(resolve: () => string | null) { foreignSpace = resolve }

const sameProjects = (current: readonly Project[], local: readonly Project[]) =>
  current === local || (foreignIds.size > 0 && current.length >= local.length && local.every((p, i) => current[i] === p))

/** The local lists plus what the other machines list. With one machine this is the local lists, untouched. */
function composeLists(localProjects: readonly Project[], localThreads: readonly StateThread[]): { projects: Project[]; threads: StateThread[] } {
  const composed = composeListsOnly(localProjects, localThreads)
  applyRemoteChildren(composed.children)
  return composed
}

/** Children another machine's threads started on this one: listed by its daemon, reached from the thread that started them. */
const originChildren = new OriginChildren()
/**
 * `live.ts` hands over every event of a child another machine started here. With several machines it is nested under the
 * thread that started it (`composeLists`) and this returns true; with one, `live.ts` keeps it as it always did.
 */
export function noteOriginChild(thread: Thread): boolean {
  const several = machineHub().machineCount > 1
  if (originChildren.put(thread) && several) remoteChanged()
  return several
}
export function forgetOriginChild(id: ThreadId) {
  if (originChildren.remove(id) && machineHub().machineCount > 1) remoteChanged()
}

/** Delegated children other machines run for threads listed here; they go to `nav.children`, not the thread list. */
let remoteChildIds: ReadonlySet<string> = new Set()
function applyRemoteChildren(children: readonly StateThread[]) {
  const next = withRemoteChildren(getNav().children, children, remoteChildIds)
  remoteChildIds = next.ids
  if (next.children) setNav({ children: next.children })
}

function composeListsOnly(localProjects: readonly Project[], localThreads: readonly StateThread[]): { projects: Project[]; threads: StateThread[]; children: StateThread[] } {
  const hub = machineHub()
  // Until the daemon answered and the machine is known, nothing can be told apart from another machine's: the local lists stand alone.
  if (hub.machineCount <= 1 || !hub.localMachineId) {
    if (foreignIds.size) { foreignIds = new Set(); machinesOf = new Map() }
    return { projects: localProjects as Project[], threads: localThreads as StateThread[], children: [] }
  }
  hub.ingestPrimaryNow()
  const merged = merger.merge({ view: hub.view(), localProjects, spaceOfForeign: foreignSpace, children: remoteChildStore.get() })
  foreignIds = new Set(merged.projects.slice(localProjects.length).map(p => p.id))
  machinesOf = merged.machinesOf
  const threads = merged.threads.length ? [...localThreads, ...merged.threads].sort(byPinnedThenNewest) : localThreads as StateThread[]
  const children = originChildren.list.length ? [...merged.children, ...originChildren.list] : merged.children
  const nested = underParents(children, threads, hub.localMachineId!)
  // What this machine's daemon records of its threads' children on other machines decides over a stale copy (ADE-121).
  const records = remoteDelegationStore.list()
  const recorded = records.length ? recordOverlay.apply(nested, records, new Map(localThreads.map(th => [th.id, th]))) : nested
  return { projects: merged.projects, threads, children: recorded }
}
const recordOverlay = new RemoteRecordOverlay()

let viewStop: (() => void) | undefined
let machinesStop: (() => void) | undefined
let remoteDirty = false
/** Another machine's lists or status changed: list them again (in a microtask, once per burst). */
function remoteChanged() {
  if (remoteDirty) return
  remoteDirty = true
  queueMicrotask(() => { remoteDirty = false; refreshMerged() })
}
function refreshMerged() {
  if (!state.listsLoaded) return
  const local = state.threads.filter(th => !th.machineId)
  const composed = composeLists(state.projects.filter(p => !foreignIds.has(p.id)), local)
  // New chats for another machine are in neither list yet; they stay until that machine lists them.
  const threads = withNewChats(composed.threads)
  if (composed.projects.length !== state.projects.length || composed.projects.some((p, i) => p !== state.projects[i]) || threads.length !== state.threads.length || threads.some((th, i) => th !== state.threads[i])) {
    set({ projects: composed.projects, threads })
  }
  // A pane restored on a thread another machine owns could not open until that machine's lists arrived.
  for (const [paneId, pane] of panes) if (pane.selectedId && !pane.open.loaded && !sessions.has(paneSessionKey(pane))) {
    const owner = machineOfThreadId(pane.selectedId)
    if (owner && owner !== pane.machineId) void selectPaneThread(paneId, pane.selectedId, false, owner).catch(error => console.error('[pane] Unable to load thread:', error))
  }
}
/** Follow the machine set: with more than one machine the merged view is kept up to date, with one nothing is read. */
function watchMachines() {
  const hub = machineHub()
  const sync = () => {
    const several = hub.machineCount > 1
    if (several && !viewStop) viewStop = hub.subscribe(remoteChanged)
    if (!several && viewStop) { viewStop(); viewStop = undefined }
    remoteChanged()
  }
  if (!machinesStop) {
    const stops = [hub.onMachinesChanged(sync), remoteChildStore.subscribe(remoteChanged), remoteDelegationStore.subscribe(remoteChanged)]
    machinesStop = () => { for (const stop of stops) stop() }
  }
  sync()
}
/** A machine that cannot be reached is a notice, not an error screen. */
function notifyOffline(error: unknown) {
  const message = error instanceof Error ? error.message : String(error)
  void import('./shell/toasts.tsx').then(({ toast }) => toast.info(message.includes('offline') ? message : 'That machine is offline. Its threads show from the last time it was online.')).catch(() => {})
}

async function loadLists() {
  if (!client) return
  const owner = client
  const initial = !state.listsLoaded
  startupMark('lists-start')
  const [p, t] = await Promise.all([
    owner.call('projects.list', initial ? { refresh_git: false } : {}),
    owner.call('threads.list', {}).then(result => {
      if (owner === client) {
        lastActivity = { owner, activity: result.activity ?? [] }
        for (const listener of activityListeners) listener(lastActivity.activity)
      }
      return result
    }),
  ])
  if (owner !== client) return
  startupMark('lists-ready', { projects: p.projects.length, threads: t.threads.length })
  listedThreads = t.threads
    .filter((th) => !isChildThread(th) && !isInboundThread(th) && th.status !== 'archived')
    .sort(byPinnedThenNewest)
  originChildren.replace(t.threads)
  // The set learns this machine's lists first, so a project the user has here is not listed again for another machine's checkout.
  machineHub().ingestPrimary(owner, p.projects, t.threads, t.activity)
  const composed = composeLists(p.projects, listedThreads)
  set({ projects: composed.projects, threads: withNewChats(composed.threads), listsLoaded: true })
  if (initial) {
    // Preserve the usual Git self-heal without putting one subprocess per
    // project on the first sidebar paint's critical path. Notifications may
    // arrive while this runs; a fresh explicit list read remains authoritative.
    void owner.call('projects.list', {}).then(result => {
      if (owner !== client || !sameProjects(state.projects, p.projects)) return
      const changed = result.projects.length !== p.projects.length || result.projects.some((project, index) => {
        const before = p.projects[index]!
        return project.id !== before.id || project.updated_at !== before.updated_at || project.is_git !== before.is_git
      })
      if (changed) { machineHub().ingestPrimary(owner, result.projects, t.threads, t.activity); set({ projects: composeLists(result.projects, state.threads.filter(th => !th.machineId)).projects }) }
    }, () => {})
  }
  for (const [paneId, pane] of panes) if (pane.selectedId && !pane.open.loaded && !sessions.has(paneSessionKey(pane))) void selectPaneThread(paneId, pane.selectedId, false, pane.machineId).catch(error => console.error('[pane] Unable to load thread:', error))
  const want = process.env.KYBERN_GPUIX_THREAD
  if (want && !state.selectedId) {
    const match = listedThreads.find((th) => th.id === want || th.title.toLowerCase().includes(want.toLowerCase()))
    if (match) void selectThread(match.id)
  }
}

const openEventListeners = new Set<(event: ThreadEvent) => void>()
/** Observe the open thread's live events after they are folded in (earlier-history replay). */
export function onOpenEvent(listener: (event: ThreadEvent) => void): () => void {
  openEventListeners.add(listener)
  return () => openEventListeners.delete(listener)
}

export function selectThread(id: ThreadId | null, machineId?: MachineId) { return selectPaneThread(focusedPaneId, id, false, machineId) }
/** `machineId` names the owner of a thread another machine listed; without it the owner is looked up (`machineOfThreadId`). */
export async function selectPaneThread(paneId: string, id: ThreadId | null, keepOpen = false, machineId?: MachineId) {
  const pane = panes.get(paneId)
  if (!pane) return
  const generation = pane.generation + 1
  const machine = id ? machineOfThreadId(id, machineId) : undefined
  const key = id ? sessionKey(id, machine) : ''
  // A repeat explicit selection refreshes a sole owner, retaining race protection.
  if (id && pane.selectedId === id && pane.machineId === machine && ![...panes].some(([other, p]) => other !== paneId && p.selectedId === id && p.machineId === machine)) {
    sessions.get(key)?.stop?.(); sessions.delete(key)
  }
  const existing = id ? sessions.get(key) : undefined
  // A new chat the daemon has not made yet opens on its own record: its first message shows as a pending bubble, and the
  // transcript loads when the create answers (`newChats.subscribe` above).
  const uncreated = id && !existing && newChats.uncreated(id) ? newChats.get(id)!.thread : null
  if (uncreated) {
    // A chat started for another machine opens on that machine's id (its composer and sends resolve the owner from it).
    setPane(paneId, { selectedId: id, machineId: newChats.get(id)!.machineId, open: { ...emptyThreadState(), thread: uncreated, loaded: true }, generation })
    releaseUnused()
    return
  }
  setPane(paneId, { selectedId: id, machineId: machine, open: existing?.open ?? (keepOpen ? pane.open : emptyThreadState()), generation })
  releaseUnused()
  if (!id || !client) return
  if (existing) { await existing.ready; return }
  let owner: KybernClient
  try { owner = machine ? machineHub().clientFor(machine) : client } catch (error) {
    // The machine is not reachable: its cached copy of the thread shows, and the transcript waits for it.
    const cached = state.threads.find(t => t.id === id && t.machineId === machine)
    if (cached) setPane(paneId, { open: { ...emptyThreadState(), thread: cached, loaded: true } })
    notifyOffline(error)
    return
  }
  const session: Session = { threadId: id, ...(machine ? { machineId: machine } : {}), open: emptyThreadState(), history: new HistoryPrefetch() }
  const discardHistory = () => { clearTimeout(session.historyTimer); session.history.clear() }
  session.stop = discardHistory
  sessions.set(key, session)
  const valid = () => (machine ? true : owner === client) && sessions.get(key) === session
  session.ready = (async () => {
    startupMark('thread-start', { id })
    const res = await owner.call('threads.get', { thread_id: id, transcript_limit: 120, include_tool_output: false })
    if (!valid()) return
    startupMark('thread-page-ready', { id, entries: res.transcript.length })
    const seeded = seedFromGet(res)
    startupMark('thread-projected', { id, blocks: seeded.blocks.length })
    setThreadOpen(id, seeded, false, machine)
    startupMark('thread-ready', { id, entries: res.transcript.length })
    memoryStep('thread-open', { id, entries: res.transcript.length, blocks: seeded.blocks.length })
    const sub = owner.subscribeEvents(
      { thread_id: id, after_seq: res.thread.last_seq, include_tool_output: false },
      event => {
        if (!valid()) return
        // A snapshot from before this event cannot repair newly exposed older
        // rows. Demand loading keeps its existing in-flight event replay.
        session.history.clear()
        setThreadOpen(id, applyEvent(session.open, event), event.kind === 'assistant_text_delta' || event.kind === 'assistant_thinking_delta', machine)
        for (const listener of threadEventListeners) listener(id, event)
        if (state.selectedId === id && state.selectedMachineId === machine) for (const listener of openEventListeners) listener(event)
      },
    )
    session.stop = () => { discardHistory(); sub.unsubscribe() }
    primeThreadHistory(id)
  })()
  try { await session.ready } catch (error) {
    if (valid()) { sessions.delete(key); throw error }
  }
}
/**
 * Hidden-window compaction (`platform/surface.ts`, the desktop's
 * `releaseCachedData`): drop every open transcript's blocks and its live
 * subscription. The thread record, selection and pending requests stay, so the
 * composer and its drafts are untouched.
 */
export function releaseOpenTranscripts() {
  for (const session of sessions.values()) session.stop?.()
  sessions.clear()
  for (const [id, pane] of panes) panes.set(id, { ...pane, open: compactThreadState(pane.open), generation: pane.generation + 1 })
  mirrorFocus(); emit()
}
/** Showing the window again rehydrates each pane's thread (the desktop's `loadOpenThreads`). */
export function reloadOpenTranscripts() {
  for (const [id, pane] of panes) if (pane.selectedId && !pane.open.loaded) void selectPaneThread(id, pane.selectedId, true).catch(error => console.error('[pane] Unable to reload thread:', error))
}
const threadEventListeners = new Set<(id: string, event: ThreadEvent) => void>()
export function onThreadEvent(listener: (id: string, event: ThreadEvent) => void) {
  threadEventListeners.add(listener)
  return () => { threadEventListeners.delete(listener) }
}

export async function send(text: string) { return sendFromPane(focusedPaneId, text) }
export async function sendFromPane(paneId: string, text: string) {
  const state = getPaneState(paneId)
  const generation = getPane(paneId)?.generation
  const body = text.trim()
  if (!body || !client) return
  const message = { parts: [{ type: 'text' as const, text: body }] }
  if (state.selectedId) {
    await (threadClient(state.selectedId, state.selectedMachineId) ?? client).call('threads.send', { thread_id: state.selectedId, message })
    return
  }
  // Home: start a thread in the draft's project, or a free chat (desktop `createThread`).
  const recent = state.threads[0]
  const owner = client
  const created = await owner.call('threads.create', {
    ...(state.draft.projectId ? { project_id: state.draft.projectId } : {}),
    provider: recent?.provider ?? { kind: 'claude-code', instance: 'default' },
    message,
  })
  if (owner !== client) return
  await refreshLists()
  if (owner === client && getPane(paneId)?.generation === generation) void selectPaneThread(paneId, created.id)
}

/** Show the home screen for a project's new thread (or a free chat without one). */
export function selectDraft(projectId?: string, purpose?: 'thread' | 'coordinator') {
  set({ draft: { ...(projectId ? { projectId } : {}), ...(purpose ? { purpose } : {}) } })
  void selectThread(null)
}

export function toggleProject(id: string) {
  set({ collapsed: { ...state.collapsed, [id]: !state.collapsed[id] } })
}

export function toggleSidebar() {
  set({ sidebarOpen: !state.sidebarOpen })
}
