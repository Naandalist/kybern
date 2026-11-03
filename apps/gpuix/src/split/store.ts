import { useSyncExternalStore } from 'react'
import { canSplitPane, collectThreadPanes, findParentSplit, findThreadPane, removeThreadPane, replacePaneThread, setSplitNodeRatio, splitThreadPane, type Pane, type SplitDirection, type SplitSide } from '../shared/state/splitView.ts'
import { createPane, closePaneState, DEFAULT_PANE, focusPane, getFocusedPaneId, getState, getPane, selectPaneThread, setPane, subscribe } from '../state.ts'
import { load, save } from '../shell/persist.ts'
import { CHAT, chatGroup, closeIn, hasTab, moveBetween, normalize, readGroup, readMemory, release, remember, retarget, tabKey, writeMemory, type Group, type GroupRecord, type Groups, type Memory, type Tab } from '../tabs/model.ts'
export { canSplitPane, collectThreadPanes }
export type { Pane, SplitDirection, SplitSide }
// One window split tree (max 2×2). Every leaf is a tab group (ADE-54): the thread it shows and that thread's tabs in this
// group (`tabs/model.ts`). Saved as `kybern.split-view.v2`; a `v1` tree (leaves without tabs) is migrated on first read,
// each leaf taking its thread's remembered tabs. What a thread had open is remembered per thread in `kybern.tabs.threads`.
let restoring = false
let started = false
const KEY_V1 = 'kybern.split-view.v1'
const KEY = 'kybern.split-view.v2'
const MEMORY_KEY = 'kybern.tabs.threads'
let root: Pane = { kind: 'leaf', id: DEFAULT_PANE, threadId: null }
let groups: Groups = {}
let memory: Memory = new Map()
/** ⌘⇧T: recently closed tabs, newest last. In memory only. */
const closed: { threadId: string; tab: Tab; leafId: string; index: number }[] = []
const listeners = new Set<() => void>()
const notify = () => { for (const listener of listeners) listener() }
const listen = (listener: () => void) => { listeners.add(listener); return () => { listeners.delete(listener) } }
const tabListeners = new Set<() => void>()
const notifyTabs = () => { for (const listener of tabListeners) listener() }
const listenTabs = (listener: () => void) => { tabListeners.add(listener); return () => { tabListeners.delete(listener) } }
export const getSplitRoot = () => root
export const useSplitRoot = () => useSyncExternalStore(listen, getSplitRoot)
export function useFocusedPane() { return useSyncExternalStore(subscribe, getFocusedPaneId) }
const NO_GROUP: GroupRecord = { threadId: null, ...chatGroup() }
export const getGroup = (leafId: string): GroupRecord => groups[leafId] ?? NO_GROUP
export const getGroups = () => groups
export function useGroup(leafId: string) { return useSyncExternalStore(listenTabs, () => getGroup(leafId)) }
export function useGroups() { return useSyncExternalStore(listenTabs, getGroups) }
export const subscribeTabs = listenTabs
/** What a thread remembers (its tabs while it is off screen). */
export const getMemory = (threadId: string): Group | undefined => memory.get(threadId)
/** Whether a tab (by `tabKey`) of the thread is open: in its groups while it is on screen, else in what it remembers. */
export function threadHasTab(threadId: string, key: string): boolean {
  const own = Object.values(groups).filter((group) => group.threadId === threadId)
  if (own.length) return own.some((group) => hasTab(group, key))
  const remembered = memory.get(threadId)
  return !!remembered && hasTab(remembered, key)
}
/** The leaves showing a thread, in tree order. */
export const groupsOfThread = (threadId: string) => collectThreadPanes(root).filter((leaf) => groups[leaf.id]?.threadId === threadId).map((leaf) => leaf.id)

function persist() {
  if (!started || restoring) return
  remember(groups, memory, getFocusedPaneId())
  save(KEY, { version: 2, root, focusedPaneId: getFocusedPaneId(), groups })
  save(MEMORY_KEY, writeMemory(memory))
}

/** Set one leaf's group. `fn` may return the same object for no change. */
export function updateGroup(leafId: string, fn: (group: Group) => Group) {
  const current = groups[leafId]
  if (!current?.threadId) return
  const next = fn(current)
  if (next === current) return
  groups = { ...groups, [leafId]: { ...next, threadId: current.threadId } }
  persist(); notifyTabs()
}

/** Change a thread's remembered tabs while it is off screen (an agent opening a preview in a background thread). */
export function updateMemory(threadId: string, fn: (group: Group) => Group) {
  const next = normalize(fn(memory.get(threadId) ?? chatGroup()))
  memory.delete(threadId); memory.set(threadId, next)
  save(MEMORY_KEY, writeMemory(memory)); notifyTabs()
}

/** Close one tab; the chat never closes. Remembered for ⌘⇧T, except a terminal: its shell closes with the tab, so reopening it
 *  would only show "This terminal has ended". */
export function closeTab(leafId: string, key: string) {
  const group = groups[leafId]
  if (!group?.threadId || key === CHAT) return
  const index = group.tabs.findIndex((tab) => tabKey(tab) === key)
  if (index < 0) return
  if (group.tabs[index]!.kind !== 'terminal') closed.push({ threadId: group.threadId, tab: group.tabs[index]!, leafId, index })
  if (closed.length > 30) closed.shift()
  // A group that held only this tab (a split-off document) closes with it.
  if (group.tabs.length === 1 && root.kind === 'split') { groups = { ...groups, [leafId]: { ...group, tabs: [] } }; closePane(leafId); return }
  updateGroup(leafId, (g) => closeIn(g, key))
}

/** The last closed tab of the leaf's thread, back where it was (in this leaf when its old group is gone). */
export function reopenClosedTab(leafId: string): boolean {
  const threadId = groups[leafId]?.threadId
  if (!threadId) return false
  for (let i = closed.length - 1; i >= 0; i--) {
    const entry = closed[i]!
    if (entry.threadId !== threadId) continue
    closed.splice(i, 1)
    const key = tabKey(entry.tab)
    if (groupsOfThread(threadId).some((id) => groups[id]!.tabs.some((tab) => tabKey(tab) === key))) continue
    const target = groups[entry.leafId]?.threadId === threadId ? entry.leafId : leafId
    updateGroup(target, (g) => { const tabs = g.tabs.slice(); tabs.splice(Math.max(1, Math.min(tabs.length, entry.index)), 0, entry.tab); return { ...g, tabs, active: key } })
    focusPane(target)
    return true
  }
  return false
}

export function splitPane(target: string, direction: SplitDirection, threadId: string | null = null, side: SplitSide = 'second', seed?: Group) {
  const result = splitThreadPane({ root, targetPaneId: target, direction, threadId, side })
  if (!result) return false
  root = result.root
  // A seeded leaf (a tab split off into its own group) starts with that group instead of the thread's memory.
  if (seed && threadId) groups = { ...groups, [result.addedPaneId]: { threadId, ...seed } }
  createPane(result.addedPaneId)
  // Select before focusing: the focus publishes, and the group must already name its thread then.
  void selectPaneThread(result.addedPaneId, threadId).catch(console.error)
  focusPane(result.addedPaneId)
  persist(); notify(); notifyTabs()
  return true
}
export const splitFocusedPane = (direction: SplitDirection, threadId?: string | null) => splitPane(getFocusedPaneId(), direction, threadId)

/**
 * ⌘\ / ⌘⇧\ on a document tab, or a tab dropped on a pane edge: the tab moves into a new group of the same thread beside
 * `target` (its own group by default). A source group the move empties closes.
 */
export function splitTab(from: string, key: string, direction: SplitDirection, side: SplitSide = 'second', target = from) {
  const group = groups[from]
  const host = groups[target]
  if (!group?.threadId || host?.threadId !== group.threadId || (target === from && group.tabs.length < 2) || !canSplitPane(root, target, direction)) return false
  const tab = group.tabs.find((t) => tabKey(t) === key)
  if (!tab) return false
  const at = group.tabs.indexOf(tab)
  const rest = group.tabs.filter((t) => t !== tab)
  const fallback = rest[Math.min(at, rest.length - 1)]
  groups = { ...groups, [from]: { ...group, tabs: rest, active: group.active === key ? fallback ? tabKey(fallback) : CHAT : group.active, preview: group.preview === key ? null : group.preview } }
  const ok = splitPane(target, direction, group.threadId, side, { tabs: [tab], active: key, preview: null })
  if (ok && !rest.length) closePane(from, true)
  return ok
}

/** A tab dropped on another group's strip. Only between groups of one thread. */
export function moveTabToGroup(from: string, key: string, to: string, index?: number) {
  const result = moveBetween(groups, from, key, to, index)
  if (result.groups === groups) return false
  groups = result.groups
  for (const id of result.emptied) closePane(id, true)
  focusPane(to)
  persist(); notifyTabs()
  return true
}

/**
 * `openThreadInSplit` (desktop state/store.ts:873), the thread context menu's "Open to the right" / "Open below": the thread opens
 * beside the focused pane. With nothing open in the focused pane (home, a draft) the thread takes that pane and an empty pane opens
 * next to it, like the desktop's `createSplitView({ sourceThreadId })`, instead of the empty pane staying first.
 */
export function openThreadInSplit(threadId: string, direction: SplitDirection): boolean {
  const focused = findThreadPane(root, getFocusedPaneId())
  if (!focused || focused.threadId) return splitFocusedPane(direction, threadId)
  if (!canSplitPane(root, focused.id, direction)) return false
  // `selectPaneThread` records the pane's selection at once; the subscription below mirrors it into the tree.
  void selectPaneThread(focused.id, threadId).catch(console.error)
  return splitPane(focused.id, direction, null)
}
/** Whether the context menu's open-beside items apply: desktop `!inSplit && (!splitView || !focusedPane?.threadId || canSplitPane(...))`. */
export function canOpenInSplit(threadId: string, direction: SplitDirection): boolean {
  if (collectThreadPanes(root).some((pane) => pane.threadId === threadId)) return false
  const focused = findThreadPane(root, getFocusedPaneId())
  return !focused?.threadId || canSplitPane(root, focused.id, direction)
}
/** Close a pane. Its tabs join another group of its thread (or the thread's memory); `discard` drops an emptied group as is. */
export function closePane(id: string, discard = false) {
  if (root.kind === 'leaf') return
  const next = removeThreadPane(root, id)
  if (!next) return
  const parent = findParentSplit(root, id)
  const sibling = parent && (parent.first.id === id ? parent.second : parent.first)
  if (getFocusedPaneId() === id && sibling) focusPane(collectThreadPanes(sibling)[0]!.id)
  root = next
  const group = groups[id]
  const rest = { ...groups }
  delete rest[id]
  groups = group?.threadId && !discard && group.tabs.length ? release(rest, memory, group.threadId, group) : rest
  closePaneState(id)
  persist(); notify(); notifyTabs()
}
export function maximizePane(id: string) {
  const target = findThreadPane(root, id)
  if (!target) return
  const others = collectThreadPanes(root).filter(pane => pane.id !== id)
  root = target
  let next: Groups = groups[id] ? { [id]: groups[id]! } : {}
  for (const pane of others) {
    const group = groups[pane.id]
    if (group?.threadId) next = release(next, memory, group.threadId, group)
    closePaneState(pane.id)
  }
  groups = next
  focusPane(id); persist(); notify(); notifyTabs()
}
export function resizePane(id: string, ratio: number) {
  const next = setSplitNodeRatio(root, id, ratio)
  if (next === root) return
  root = next; persist(); notify()
}

/** Each leaf's selection drives its group: a pane that shows another thread swaps its tabs (`retarget`). */
function reconcile(): boolean {
  let changed = false
  const emptied: string[] = []
  for (const leaf of collectThreadPanes(root)) {
    const selected = getPane(leaf.id)?.selectedId ?? null
    if (groups[leaf.id] && groups[leaf.id]!.threadId === selected) continue
    const result = retarget(groups, memory, leaf.id, selected)
    groups = result.groups; emptied.push(...result.emptied); changed = true
  }
  // Groups of leaves that left the tree.
  const leaves = new Set(collectThreadPanes(root).map((leaf) => leaf.id))
  for (const id of Object.keys(groups)) if (!leaves.has(id)) { const { [id]: gone, ...rest } = groups; groups = gone?.threadId ? release(rest, memory, gone.threadId, gone) : rest; changed = true }
  if (emptied.length) queueMicrotask(() => { for (const id of emptied) if (findThreadPane(root, id)) closePane(id, true) })
  return changed
}

let persistedFocus = getFocusedPaneId()
subscribe(() => {
  if (!started || restoring) return
  let next = root
  for (const pane of collectThreadPanes(root)) next = replacePaneThread(next, pane.id, getPane(pane.id)?.selectedId ?? null)
  const regrouped = reconcile()
  const changed = next !== root || persistedFocus !== getFocusedPaneId() || regrouped
  persistedFocus = getFocusedPaneId()
  if (next !== root) { root = next; notify() }
  if (regrouped) notifyTabs()
  if (changed) persist()
})
function valid(value: unknown, depth = 0, ids = new Set<string>()): value is Pane {
  if (!value || typeof value !== 'object' || depth > 2) return false
  const p = value as Pane
  if (typeof p.id !== 'string' || ids.has(p.id)) return false
  ids.add(p.id)
  if (p.kind === 'leaf') return p.threadId === null || typeof p.threadId === 'string'
  return p.kind === 'split' && (p.direction === 'horizontal' || p.direction === 'vertical') && Number.isFinite(p.ratio) && p.ratio >= .25 && p.ratio <= .75 && valid(p.first, depth + 1, ids) && valid(p.second, depth + 1, ids)
}
type Stored = { version: number; root: Pane; focusedPaneId: string; groups?: Record<string, unknown> }
/**
 * Saved groups for a restored tree. A v2 group is kept when it names its leaf's thread; anything else (a v1 tree, a damaged
 * group) starts from the thread's memory. One tab lives in one group: a key seen in an earlier leaf is dropped from later ones.
 */
export function restoreGroups(tree: Pane, stored: Record<string, unknown> | undefined, remembered: Memory): Groups {
  const out: Groups = {}
  const seen = new Set<string>()
  for (const leaf of collectThreadPanes(tree)) {
    if (!leaf.threadId) { out[leaf.id] = { threadId: null, ...chatGroup() }; continue }
    const raw = stored?.[leaf.id] as { threadId?: unknown } | undefined
    const saved = raw && raw.threadId === leaf.threadId ? readGroup(raw) : null
    const fresh = !saved && !Object.values(out).some((g) => g.threadId === leaf.threadId)
    const base = saved ?? (fresh ? normalize(remembered.get(leaf.threadId) ?? chatGroup()) : null)
    const tabs = (base?.tabs ?? []).filter((tab) => !seen.has(`${leaf.threadId}\u0000${tabKey(tab)}`))
    if (!tabs.length) { out[leaf.id] = { threadId: leaf.threadId, ...chatGroup() }; continue }
    for (const tab of tabs) seen.add(`${leaf.threadId}\u0000${tabKey(tab)}`)
    const active = tabs.some((tab) => tabKey(tab) === base!.active) ? base!.active : tabKey(tabs[0]!)
    out[leaf.id] = { threadId: leaf.threadId, tabs, active, preview: base!.preview && tabs.some((tab) => tabKey(tab) === base!.preview) ? base!.preview : null }
  }
  return out
}
function restore() {
  memory = readMemory(load(MEMORY_KEY, []))
  closed.length = 0
  const stored = load<Stored | null>(KEY, null) ?? load<Stored | null>(KEY_V1, null)
  root = { kind: 'leaf', id: DEFAULT_PANE, threadId: null }
  groups = {}
  if ((stored?.version !== 1 && stored?.version !== 2) || !valid(stored.root)) { restoring = false; reconcile(); notify(); notifyTabs(); return }
  restoring = true
  // Do not persist the intermediate empty panes while restoring selection.
  const saved = stored.root
  for (const leaf of collectThreadPanes(saved)) createPane(leaf.id)
  for (const leaf of collectThreadPanes(saved)) setPane(leaf.id, { selectedId: leaf.threadId })
  root = saved
  groups = restoreGroups(saved, stored.version === 2 ? stored.groups : undefined, memory)
  if (!findThreadPane(root, DEFAULT_PANE)) closePaneState(DEFAULT_PANE)
  focusPane(findThreadPane(root, stored.focusedPaneId)?.id ?? collectThreadPanes(root)[0]!.id)
  restoring = false
  if (getState().status === 'open') for (const leaf of collectThreadPanes(root)) if (leaf.threadId) void selectPaneThread(leaf.id, leaf.threadId).catch(console.error)
  // v2 is written beside the v1 entry, which is left as it was, so an older build still finds its layout.
  persist(); notify(); notifyTabs()
}
export function startSplitView() { if (!started) { started = true; restore() } }
