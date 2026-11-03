import { applyUsageLimits as applyLimits } from './limits.ts'
import { environmentKey } from '../shell/environment.ts'
// Keeps the thread panel live, like the desktop's `state/rpc.ts`:
//
// - one events subscription for every thread folds `thread_created`,
//   `thread_updated` and `thread_archived` into the lists, so status glyphs,
//   pins, titles and new threads appear without a refetch;
// - provider subagents and delegated children (left out of `threads.list`) are
//   fetched per parent while it has agents at work, and then follow events;
// - background activity (working, monitoring) comes from `threads.list`, re-read
//   shortly after runtime tasks change;
// - a turn that completes while its thread is not open marks it unread;
// - plan limits follow `usage.limits` and `usage.limits.changed`;
// - the other machines' threads follow their own event streams (`remoteLive.ts`), only while another machine is connected.
//
// Text deltas cost one switch here and nothing else: no state changes, no render.
import { mkdirSync, readFileSync, renameSync, writeFileSync } from 'node:fs'
import path from 'node:path'
import { createHash } from 'node:crypto'
import { isChildThread, isRemoteChildThread } from '../../../../packages/kybern-client/src/subagents.ts'
import { isChildWorking } from '../../../../packages/kybern-client/src/delegations.ts'
import {
  USAGE_LIMITS_CHANGED_NOTIFICATION,
  type Thread,
  type ThreadActivitySummary,
  type ThreadEvent,
  type ThreadId,
  type UsageLimitsResult,
} from '../../../../packages/kybern-client/src/types.ts'
import { forgetOriginChild, getClient, getState, noteOriginChild, onThreadActivity, set, subscribe } from '../state.ts'
import { DEFAULT_FILTER, getNav, setNav, subscribeNav, type NavState } from './store.ts'
import { followSpaces, loadSpaces } from '../spaces/store.ts'
import { followWorkspaceProgress } from '../groups/store.ts'
import { gpuixConfigDir } from '../platform/os.ts'
import { machineHub } from '../machines/index.ts'
import { RemoteLive, activityMaps, withRemote } from './remoteLive.ts'
import { remoteChildStore } from './remoteChildren.ts'
import { RemoteDelegations, remoteDelegationStore } from './remoteDelegations.ts'

const LIMITS_POLL_MS = 60_000
const ACTIVITY_DEBOUNCE_MS = 600

const byRecent = (a: Thread, b: Thread) => Number(b.pinned) - Number(a.pinned) || b.updated_at.localeCompare(a.updated_at)

/** Fields a sidebar row shows; children re-render only when one of these changes (`state/subagents.ts` `sidebarKey`). */
const childKey = (t: Thread) =>
  `${t.id}|${t.title}|${t.status}|${t.parent_thread_id ?? ''}|${t.provider.kind}|${t.subagent?.status}|${t.subagent?.started_at}|${t.subagent?.completed_at ?? ''}|${t.delegation?.status ?? ''}|${t.delegation?.started_at ?? ''}|${t.delegation?.completed_at ?? ''}`

/** Fields the panel, title bar and palette show for a top-level thread. */
const threadKey = (t: Thread) =>
  `${t.title}|${t.status}|${t.pinned}|${t.updated_at}|${t.project_id}|${t.coordinator_project_id ?? ''}|${t.worktree?.branch ?? ''}|${t.provider.kind}|${t.model ?? ''}|${t.effort ?? ''}|${t.permission_mode}`

/** Threads archived here or by an event: a late `thread_updated` must not bring their rows back. */
const archived = new Set<ThreadId>()
export function markArchived(id: ThreadId) {
  archived.add(id)
}

function upsertThread(thread: Thread) {
  if (thread.status === 'archived') archived.add(thread.id)
  else if (archived.has(thread.id)) return
  if (isChildThread(thread)) {
    // A child a thread of another machine started here is nested under that thread with the merged lists (`state.ts`).
    if (isRemoteChildThread(thread) && noteOriginChild(thread)) return
    const children = getNav().children
    const current = children[thread.id]
    if (thread.status === 'archived') {
      if (!current) return
      const next = { ...children }
      delete next[thread.id]
      return setNav({ children: next })
    }
    if (current && childKey(current) === childKey(thread)) {
      // Progress only: keep the newest object without waking the panel.
      children[thread.id] = thread
      return
    }
    // A child that left after finishing and now works again (a delegated child re-woken by a message) is listed again,
    // and holds its check again when it next settles.
    const dismissed = getNav().dismissed
    if (dismissed[thread.id] && isChildWorking(thread)) {
      const kept = { ...dismissed }
      delete kept[thread.id]
      return setNav({ children: { ...children, [thread.id]: thread }, dismissed: kept })
    }
    return setNav({ children: { ...children, [thread.id]: thread } })
  }
  // This is the machine the user is at: a thread of another machine with the same id is not this one (`remoteLive.ts`).
  const threads = getState().threads
  const index = threads.findIndex((t) => t.id === thread.id && !t.machineId)
  if (index >= 0 && threads[index]!.last_seq > thread.last_seq) return
  // An update that changes nothing the lists show keeps the same array: no commit, no layout.
  if (index >= 0 && threadKey(threads[index]!) === threadKey(thread)) return
  if (thread.status === 'archived') {
    if (index >= 0) set({ threads: threads.filter((t, at) => at !== index) })
    return
  }
  const next = index >= 0 ? threads.map((t, at) => (at === index ? thread : t)) : [thread, ...threads]
  set({ threads: next.sort(byRecent) })
}

/** Other surfaces that follow every thread event (agent toasts). */
const eventTaps = new Set<(event: ThreadEvent) => void>()
export function onLiveEvent(listener: (event: ThreadEvent) => void): () => void {
  eventTaps.add(listener)
  return () => eventTaps.delete(listener)
}

function onEvent(event: ThreadEvent) {
  for (const tap of eventTaps) tap(event)
  switch (event.kind) {
    case 'thread_created':
    case 'thread_updated':
      return upsertThread(event.thread)
    case 'thread_archived': {
      archived.add(event.thread_id)
      forgetOriginChild(event.thread_id)
      const threads = getState().threads
      if (threads.some((t) => t.id === event.thread_id && !t.machineId)) set({ threads: threads.filter((t) => t.id !== event.thread_id || t.machineId) })
      if (getNav().children[event.thread_id]) {
        const children = { ...getNav().children }
        delete children[event.thread_id]
        setNav({ children })
      }
      return
    }
    case 'turn_completed': {
      // A finished turn the reader did not watch (desktop `announce` → "done").
      if (event.stop_reason !== 'completed') return
      const thread = getState().threads.find((t) => t.id === event.thread_id && !t.machineId)
      if (!thread || thread.parent_thread_id || (getState().selectedId === event.thread_id && !getState().selectedMachineId)) return
      if (!getNav().unread[event.thread_id]) setNav({ unread: { ...getNav().unread, [event.thread_id]: true } })
      return
    }
    // A request raised or answered anywhere (a delegated agent's included) changes which rows say "Needs input".
    case 'approval_requested':
    case 'user_input_requested':
    case 'approval_resolved':
      scheduleActivity()
      return
    case 'runtime_task_started':
    case 'runtime_task_updated':
    case 'runtime_task_completed':
      if (event.task.kind === 'agent' && event.kind === 'runtime_task_started') void loadChildren(event.thread_id)
      scheduleActivity()
      return
  }
}

const childLoads = new Map<ThreadId, Promise<void>>()
/** A thread's subagent and delegated children (`loadSubagents`). Live changes arrive as events. */
function loadChildren(parentId: ThreadId): Promise<void> {
  const pending = childLoads.get(parentId)
  if (pending) return pending
  const client = getClient()
  if (!client) return Promise.resolve()
  const request = client
    .call('threads.list', { parent_thread_id: parentId })
    .then((result) => {
      if (getClient() !== client) return
      let changed = false
      const children = { ...getNav().children }
      for (const thread of result.threads) {
        if (!isChildThread(thread) || thread.status === 'archived') continue
        const current = children[thread.id]
        if (current && current.last_seq > thread.last_seq) continue
        if (!current || childKey(current) !== childKey(thread)) changed = true
        children[thread.id] = thread
      }
      if (changed) setNav({ children })
    })
    .catch(() => {})
    .finally(() => childLoads.delete(parentId))
  childLoads.set(parentId, request)
  return request
}

/** What the machine the user is at reported; `publishActivity` adds the other machines'. */
let localActivity: NavState['activity'] = {}
let localPending: NavState['pending'] = {}
let remote: RemoteLive | undefined
/** This daemon's records of its threads' children on other machines (`remoteDelegations.ts`). */
let delegations: RemoteDelegations | undefined

function applyActivity(summaries: ThreadActivitySummary[] | undefined) {
  const maps = activityMaps(summaries)
  localActivity = maps.activity
  localPending = maps.pending
  // Delegated children are no runtime tasks of their parent: they count in `active_children` (ADE-74), children on
  // other machines included, whose records this daemon keeps.
  for (const summary of summaries ?? []) {
    if (summary.active_agents > 0 || (summary.active_children ?? 0) > 0) void loadChildren(summary.thread_id)
    if ((summary.active_children ?? 0) > 0 && machineHub().machineCount > 1) void delegations?.ensure(summary.thread_id)
  }
  publishActivity()
}

function publishActivity() {
  const activity = remote ? withRemote(localActivity, remote.activity) : localActivity
  const pending = remote ? withRemote(localPending, remote.pending) : localPending
  const current = getNav().activity
  const same =
    Object.keys(activity).length === Object.keys(current).length &&
    Object.entries(activity).every(([id, value]) => current[id] === value)
  if (!same) setNav({ activity })
  if (JSON.stringify(pending) !== JSON.stringify(getNav().pending)) setNav({ pending })
}

let activityTimer: ReturnType<typeof setTimeout> | undefined
function scheduleActivity() {
  clearTimeout(activityTimer)
  activityTimer = setTimeout(() => void refreshActivity(), ACTIVITY_DEBOUNCE_MS)
}

async function refreshActivity() {
  const client = getClient()
  if (!client) return
  try {
    const result = await client.call('threads.list', {})
    if (getClient() === client) applyActivity(result.activity)
  } catch {
    // The next runtime task change tries again.
  }
}

export async function loadProviders(forceRefresh = false) {
  const client = getClient()
  if (!client) return []
  const result = await client.call('providers.list', forceRefresh ? { force_refresh: true } : {})
  if (getClient() === client) setNav({ providers: result.providers, providersLoaded: true })
  return result.providers
}

/** Ask the daemon for plan limits; `refresh` re-reads every provider now (the card does on open). */
export function askLimits(refresh = false) {
  const client = getClient()
  if (!client) return
  void client.call('usage.limits', { cached: true, ...(refresh ? { refresh: true } : {}) }).then((result) => { if (getClient() === client) applyLimits(result) }, () => {})
}

// ---- the reader's sidebar preferences, kept across launches per daemon ----

/** The sidebar-preferences file of an environment key (`environmentKey()`), so forgetting an environment can remove it. */
export const prefsFileFor = (key: string) => path.join(
  gpuixConfigDir(),
  `workspace-${createHash('sha1').update(key).digest('hex').slice(0, 12)}.json`,
)
const prefsFile = () => prefsFileFor(environmentKey())

interface Prefs {
  collapsed?: Record<string, boolean>
  projectOrder?: string[]
  machineFilter?: string | null
  filter?: NavState['filter']
  sidebarOpen?: boolean
  unread?: NavState['unread']
  attentionDismissals?: NavState['attentionDismissals']
  spaceId?: string
  spaceViews?: NavState['spaceViews']
}

function readPrefs(): Prefs {
  try {
    return JSON.parse(readFileSync(prefsFile(), 'utf8')) as Prefs
  } catch {
    return {}
  }
}

let prefsTimer: ReturnType<typeof setTimeout> | undefined
let lastPrefs = ''
let pendingPrefs: (() => void) | undefined
let stops: (() => void)[] = []
function persistPrefs() {
  const prefs: Prefs = {
    collapsed: getState().collapsed,
    projectOrder: getNav().projectOrder,
    machineFilter: getNav().machineFilter,
    filter: getNav().filter,
    sidebarOpen: getState().sidebarOpen,
    unread: getNav().unread,
    attentionDismissals: getNav().attentionDismissals,
    ...(getNav().spaceId ? { spaceId: getNav().spaceId! } : {}),
    spaceViews: getNav().spaceViews,
  }
  const text = JSON.stringify(prefs)
  if (text === lastPrefs) return
  lastPrefs = text
  clearTimeout(prefsTimer)
  const target = prefsFile()
  pendingPrefs = () => {
    try {
      mkdirSync(path.dirname(target), { recursive: true })
      const temp = `${target}.${process.pid}.tmp`
      writeFileSync(temp, text)
      renameSync(temp, target)
    } catch {
      // Preferences are a convenience; a read-only home must not break the app.
    }
  }
  prefsTimer = setTimeout(() => { pendingPrefs?.(); pendingPrefs = undefined }, 300)
}

process.on('exit', () => pendingPrefs?.())

let started = false
let owner: ReturnType<typeof getClient> = null
let limitsTimer: ReturnType<typeof setInterval> | undefined
/** Follow a fresh client after a reconnect (`shell/connection.ts`). */
export function restartLive() {
  stopLive()
  startLive()
}

/** Start following the daemon. Called once by the app shell, after `connect()`. */
export function startLive() {
  const client = getClient()
  if (!client) return
  if (started && owner === client) return
  if (started) stopLive()
  owner = client
  started = true

  const prefs = readPrefs()
  lastPrefs = JSON.stringify(prefs)
  set({ collapsed: prefs.collapsed ?? {}, ...(prefs.sidebarOpen === undefined ? {} : { sidebarOpen: prefs.sidebarOpen }) })
  setNav({ projectOrder: prefs.projectOrder ?? [], machineFilter: prefs.machineFilter ?? null, filter: prefs.filter ?? DEFAULT_FILTER, unread: prefs.unread ?? {}, attentionDismissals: prefs.attentionDismissals ?? {}, spaceId: prefs.spaceId ?? null, spaceViews: prefs.spaceViews ?? {} })
  stops.push(subscribe(persistPrefs), subscribeNav(persistPrefs))

  const events = client.subscribeEvents({ include_tool_output: false }, (event) => { if (getClient() === client) onEvent(event) })
  stops.push(() => events.unsubscribe())
  remote = new RemoteLive({
    hub: machineHub(),
    children: remoteChildStore,
    state: getState,
    unread: () => getNav().unread,
    setUnread: (unread) => setNav({ unread }),
    publish: publishActivity,
  })
  remote.start()
  stops.push(() => { remote?.stop(); remote = undefined; localActivity = {}; localPending = {} })
  delegations = new RemoteDelegations(remoteDelegationStore, {
    call: (parentId) => client.call('delegations.remote_list', { parent_thread_id: parentId }),
    onNotification: (method, handler) => client.onNotification(method, handler),
    refreshMachine: (machineId) => void machineHub().refreshMachineLists(machineId),
  })
  delegations.start()
  // Records belong to this daemon: another one (an environment switch) starts from its own.
  stops.push(() => { delegations?.dispose(); delegations = undefined; if (getClient() !== client) remoteDelegationStore.clear() })
  // An open thread of another machine reads its children there, so its sidebar rows and launch cards can open them.
  let opened = ''
  stops.push(subscribe(() => {
    const { selectedId, selectedMachineId } = getState()
    const key = selectedId && selectedMachineId ? `${selectedMachineId}/${selectedId}` : ''
    if (key === opened) return
    opened = key
    if (key) void remote?.loadChildren(selectedMachineId!, selectedId!)
  }))
  // An open thread here reads what its daemon records of its children on other machines.
  let openedHere = ''
  stops.push(subscribe(() => {
    const { selectedId, selectedMachineId } = getState()
    const id = selectedId && !selectedMachineId ? selectedId : ''
    if (id === openedHere) return
    openedHere = id
    if (id && machineHub().machineCount > 1) void delegations?.ensure(id)
  }))
  // A listed child on another machine that still says it works: its parent's records say whether it really does (its
  // machine may be offline here, its end missed while the app was closed).
  let childrenSeen: NavState['children'] | undefined
  stops.push(subscribeNav(() => {
    const children = getNav().children
    if (children === childrenSeen || !delegations) return
    childrenSeen = children
    for (const child of Object.values(children) as (Thread & { machineId?: string })[]) {
      const info = child.delegation
      if (child.machineId && info?.status === 'running' && !remoteDelegationStore.get(info.task_id)) void delegations.ensure(info.parent_thread_id)
    }
  }))
  stops.push(followSpaces())
  stops.push(followWorkspaceProgress())
  stops.push(client.onNotification(USAGE_LIMITS_CHANGED_NOTIFICATION, (params) => applyLimits(params as UsageLimitsResult)))
  stops.push(onThreadActivity(activity => { if (getClient() === client) applyActivity(activity) }))
  const onOpen = () => {
    void loadProviders().catch(() => { if (getClient() === client) setNav({ providersLoaded: true }) })
    askLimits()
    void loadSpaces()
    void client.call('daemon.info', {}).then((info) => { if (getClient() === client) setNav({ version: info.version ?? null }) }, () => {})
    // Records that changed while the daemon was away (or before this app started) are read again.
    const { selectedId, selectedMachineId } = getState()
    void delegations?.reread(selectedId && !selectedMachineId ? [selectedId] : [])
  }
  stops.push(client.onStatus((status) => {
    if (status === 'open' && getClient() === client) onOpen()
  }))
  if (client.status === 'open') onOpen()
  // The rail rings stay current while the window is up (`usageLimits.ts` polls each minute).
  limitsTimer ??= setInterval(() => askLimits(), LIMITS_POLL_MS)
}

/** Keep the open child thread listed even after it settles (`visibleSubagentThreads` selectedId). */
export function ensureChild(id: ThreadId) {
  const thread = getNav().children[id]
  if (thread?.parent_thread_id) void loadChildren(thread.parent_thread_id)
}

function stopLive() {
  for (const stop of stops) stop()
  stops = []
  clearTimeout(prefsTimer); pendingPrefs?.(); pendingPrefs = undefined
  clearTimeout(activityTimer); clearInterval(limitsTimer); limitsTimer = undefined
  started = false; owner = null; lastPrefs = ''; childLoads.clear(); archived.clear()
}
