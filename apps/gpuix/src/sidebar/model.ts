// The thread panel as one flat list of rows for `<virtual-list>`, built with the
// desktop's rules (`views/Sidebar.tsx` `ThreadSidebar` + `ProjectItem`):
// projects in the reader's order, the coordinator first, pinned then recent
// threads, at most 8 top-level rows until "Show N more", working children
// nested under their parents, filters and the bell's attention view, then
// Recents (free chats).
import { orderProjects, threadMatchesFilter } from '@/state/sidebarOrganize'
import { nestedThreadRows } from '../../../../packages/kybern-client/src/threadRows.ts'
import { isChildThread, visibleSubagentThreads } from '../../../../packages/kybern-client/src/subagents.ts'
import { isChildWorking } from '../../../../packages/kybern-client/src/delegations.ts'
import { inSpace } from '../../../../packages/kybern-client/src/spaces.ts'
import { attentionItems } from '../../../../packages/kybern-client/src/threadAttention.ts'
import { flatTree } from '../tree/geometry.ts'
import {
  FREE_CHAT_PROJECT_ID,
  isFreeChatProject,
  type Project,
  type ProviderKind,
  type Thread,
  type ThreadActivityState,
} from '../../../../packages/kybern-client/src/types.ts'
import type { NavState, SidebarFilter } from '../nav/store.ts'

export const MAX_PROJECT_THREADS = 8

export type Row =
  /** `text`: the active space's name when there are two or more; absent says "Projects". */
  | { kind: 'label'; key: string; text?: string }
  | { kind: 'message'; key: string; text: string; clearFilter?: boolean }
  | {
      kind: 'project'
      key: string
      project: Project
      first: boolean
      open: boolean
      active: boolean
      status: 'awaiting-approval' | 'running' | 'idle' | null
      activity: ThreadActivityState | undefined
      hasDraft: boolean
      canMoveUp: boolean
      canMoveDown: boolean
    }
  | { kind: 'create-coordinator'; key: string; project: Project }
  | {
      kind: 'thread'
      key: string
      thread: Thread
      depth: number
      childCount: number
      childrenOpen: boolean
      nested: boolean
      /** Tree connectors (`flatTree`): the last of its siblings, and a bit per outer level whose stem runs on past it. */
      last?: boolean
      rails?: number
    }
  | { kind: 'child'; key: string; thread: Thread; depth: number; childCount: number; childrenOpen: boolean; last?: boolean; rails?: number }
  | { kind: 'more'; key: string; projectId: string; count: number; showAll: boolean }
  | { kind: 'no-threads'; key: string }
  /** A space without projects: drag one onto it, or add a folder. `onlySpace` drops the drag hint. */
  | { kind: 'empty-space'; key: string; onlySpace: boolean }
  | { kind: 'recents'; key: string; open: boolean }

/** Rows that finished and left (or were dismissed). A child that works again (a delegated child re-woken by a message)
 *  comes back: a dismissal ends that child's last run, not the child. */
export function dismissedChildren(nav: Pick<NavState, 'dismissed' | 'children'>, selectedId?: string | null): Set<string> {
  const ids = new Set<string>()
  for (const id of Object.keys(nav.dismissed)) {
    // The child being viewed stays listed, even if this window dismissed it earlier (a remote one finishes while another window holds it open).
    if (id === selectedId) continue
    const child = nav.children[id]
    if (!child || !isChildWorking(child)) ids.add(id)
  }
  return ids
}

export function isFiltering(filter: SidebarFilter) {
  return filter.threads !== 'all' || filter.agent !== null
}

const PROVIDER_LABEL: Record<string, string> = {
  'claude-code': 'Claude Code',
  codex: 'Codex',
  opencode: 'OpenCode',
  pi: 'pi',
  omp: 'Oh My Pi',
  cursor: 'Cursor',
  'grok-build': 'Grok Build',
}
export const providerLabel = (kind: string) => PROVIDER_LABEL[kind] ?? kind

/** "Pinned", "Codex", or "Pinned Codex". */
export function filterSummary(filter: SidebarFilter): string {
  const agent = filter.agent ? providerLabel(filter.agent) : null
  const threads = filter.threads === 'pinned' ? 'Pinned' : filter.threads === 'working' ? 'Working' : null
  return [threads, agent].filter(Boolean).join(' ')
}

export function filterEmptyText(filter: SidebarFilter): string {
  const agent = filter.agent ? `${providerLabel(filter.agent)} ` : ''
  if (filter.threads === 'pinned') return `No pinned ${agent}threads`
  if (filter.threads === 'working') return `No ${agent}threads are working`
  return `No ${agent}threads`
}

export interface SidebarInput {
  projects: Project[]
  /** Top-level, unarchived threads, pinned first then most recent. */
  threads: Thread[]
  selectedId: string | null
  draft: { projectId?: string; purpose?: string } | null
  collapsed: Record<string, boolean>
  showAll: Record<string, boolean>
  expanded: Record<string, boolean>
  nav: Pick<
    NavState,
    'children' | 'activity' | 'unread' | 'dismissed' | 'filter' | 'attentionFilter' | 'projectOrder' | 'composerDrafts' | 'attentionDismissals'
  > & Partial<Pick<NavState, 'pending'>>
  now: number
  /** The active space; null when the daemon has no spaces. `name` is set only with two or more spaces. */
  space?: { id: string | null; name: string | null; count: number }
}

/** Agents to filter by: the ones that have threads, plus the chosen one. */
export function threadAgents(threads: readonly Thread[], filter: SidebarFilter): ProviderKind[] {
  const kinds = new Set<ProviderKind>()
  for (const thread of threads) kinds.add(thread.provider.kind)
  if (filter.agent) kinds.add(filter.agent)
  return [...kinds].sort((a, b) => providerLabel(a).localeCompare(providerLabel(b)))
}

export function buildRows(input: SidebarInput): Row[] {
  const { nav } = input
  const filtering = isFiltering(nav.filter)
  const rows: Row[] = []

  // Threads the bell and the filter both allow; null when neither is on.
  let shown: Set<string> | null = null
  if (nav.attentionFilter || filtering) {
    const attention = nav.attentionFilter ? new Set(attentionItems(input.threads, nav).map((i) => i.thread.id)) : null
    shown = new Set()
    for (const thread of input.threads) {
      if (attention && !attention.has(thread.id)) continue
      if (filtering && !threadMatchesFilter(thread, nav.filter, nav.activity[thread.id])) continue
      shown.add(thread.id)
    }
  }

  const byProject = new Map<string, Thread[]>()
  const free: Thread[] = []
  for (const thread of input.threads) {
    if (isFreeChatProject(thread.project_id)) {
      if (!shown || shown.has(thread.id)) free.push(thread)
      continue
    }
    const list = byProject.get(thread.project_id) ?? []
    list.push(thread)
    byProject.set(thread.project_id, list)
  }
  const childrenByProject = new Map<string, Thread[]>()
  for (const child of Object.values(nav.children)) {
    const list = childrenByProject.get(child.project_id) ?? []
    list.push(child)
    childrenByProject.set(child.project_id, list)
  }

  const spaceId = input.space?.id ?? null
  const ordered = orderProjects(input.projects.filter((p) => !isFreeChatProject(p.id) && inSpace(p.id, input.projects, spaceId)), nav.projectOrder)
  const visibleProjects = shown
    ? ordered.filter((p) => (byProject.get(p.id) ?? []).some((t) => shown!.has(t.id)) || input.threads.some((t) => shown!.has(t.id) && t.coordinator_project_id === p.id))
    : ordered

  rows.push({ kind: 'label', key: '__projects', ...(input.space?.name ? { text: input.space.name } : {}) })
  if (ordered.length === 0) {
    if (spaceId !== null) rows.push({ kind: 'empty-space', key: '__empty-space', onlySpace: (input.space?.count ?? 0) < 2 })
    else rows.push({ kind: 'message', key: '__none', text: 'No projects yet. Add a folder to see its threads here.' })
  } else if (shown && shown.size === 0 && nav.attentionFilter && !filtering) {
    rows.push({ kind: 'message', key: '__caught-up', text: "You're all caught up. No threads need attention." })
  } else if (filtering && visibleProjects.length === 0) {
    rows.push({ kind: 'message', key: '__filtered', text: `${filterEmptyText(nav.filter)}${free.length > 0 ? ' in projects' : ''}.`, clearFilter: true })
  } else {
    visibleProjects.forEach((project, index) => {
      const threads = byProject.get(project.id) ?? []
      const coordinator = input.threads.find((t) => t.coordinator_project_id === project.id)
      const open = shown ? true : !input.collapsed[project.id]
      const running = threads.some((t) => t.status === 'running')
      const waiting = threads.some((t) => t.status === 'awaiting-approval' || nav.pending?.[t.id]?.child)
      const activity = threads.some((t) => nav.activity[t.id] === 'working')
        ? 'working'
        : threads.some((t) => nav.activity[t.id] === 'monitoring')
          ? 'monitoring'
          : undefined
      rows.push({
        kind: 'project',
        key: project.id,
        project,
        first: index === 0,
        open,
        active: !input.selectedId && input.draft?.projectId === project.id,
        status: waiting ? 'awaiting-approval' : running ? 'running' : activity ? 'idle' : null,
        activity,
        hasDraft: Object.keys(nav.composerDrafts).some((key) => key.startsWith(`project:${project.id}:`)),
        canMoveUp: index > 0,
        canMoveDown: index < visibleProjects.length - 1,
      })
      if (!open) return

      const shownChildren = visibleSubagentThreads(childrenByProject.get(project.id) ?? [], {
        selectedId: input.selectedId,
        dismissed: dismissedChildren(nav, input.selectedId),
        now: input.now,
      })
      const withChildren = new Set(shownChildren.map((t) => t.parent_thread_id))
      const defaultOpen = (thread: Thread) => withChildren.has(thread.id)
      let nested
      if (shown) {
        const filtered: Thread[] = threads.filter((t) => shown!.has(t.id))
        const allowed = new Set(filtered.map((t) => t.id))
        for (let grew = true; grew; ) {
          grew = false
          for (const child of shownChildren) {
            if (!allowed.has(child.id) && child.parent_thread_id && allowed.has(child.parent_thread_id)) {
              allowed.add(child.id)
              filtered.push(child)
              grew = true
            }
          }
        }
        nested = nestedThreadRows(filtered, input.expanded, input.selectedId ?? undefined, defaultOpen)
      } else {
        const list = coordinator ? [coordinator, ...threads.filter((t) => t.id !== coordinator.id)] : threads
        nested = nestedThreadRows([...list, ...shownChildren], input.expanded, input.selectedId ?? undefined, defaultOpen)
      }

      if (!coordinator && !shown) rows.push({ kind: 'create-coordinator', key: `${project.id}:coordinator`, project })
      const topRows = nested.filter((row) => !isChildThread(row.thread)).length
      const showAll = !!input.showAll[project.id] || !!shown
      let counted = 0
      let cutting = false
      for (const row of nested) {
        if (!showAll) {
          if (isChildThread(row.thread)) {
            if (cutting) continue
          } else {
            cutting = counted >= MAX_PROJECT_THREADS
            if (cutting) continue
            counted += 1
          }
        }
        rows.push(
          isChildThread(row.thread)
            ? { kind: 'child', key: row.thread.id, thread: row.thread, depth: row.depth, childCount: row.childCount, childrenOpen: row.open }
            : { kind: 'thread', key: row.thread.id, thread: row.thread, depth: row.depth, childCount: row.childCount, childrenOpen: row.open, nested: true },
        )
      }
      if (!shown && topRows > MAX_PROJECT_THREADS)
        rows.push({ kind: 'more', key: `${project.id}:more`, projectId: project.id, count: topRows - MAX_PROJECT_THREADS, showAll: !!input.showAll[project.id] })
      if (threads.length === 0) rows.push({ kind: 'no-threads', key: `${project.id}:empty` })
    })
  }

  const freeDraftSelected = !input.selectedId && !!input.draft && !input.draft.projectId
  if (free.length > 0 || (!nav.attentionFilter && freeDraftSelected)) {
    const open = shown ? true : !input.collapsed[FREE_CHAT_PROJECT_ID]
    rows.push({ kind: 'recents', key: '__recents', open })
    if (open)
      for (const thread of [...free].sort((a, b) => b.updated_at.localeCompare(a.updated_at)).slice(0, MAX_PROJECT_THREADS))
        rows.push({ kind: 'thread', key: thread.id, thread, depth: 0, childCount: 0, childrenOpen: false, nested: false })
  }
  return withTree(rows)
}

/** Connector facts for nested rows; every other row (headers, "Show more", Recents) closes the open levels. */
function withTree(rows: Row[]): Row[] {
  const tree = flatTree(rows.map((row) => (row.kind === 'child' || (row.kind === 'thread' && row.nested) ? row.depth : 0)))
  rows.forEach((row, i) => {
    if (row.kind !== 'child' && !(row.kind === 'thread' && row.nested)) return
    row.last = tree[i]!.last
    row.rails = tree[i]!.continues.reduce((bits, on, level) => (on ? bits | (1 << level) : bits), 0)
  })
  return rows
}
