// The delegating machine's own record of the agents its threads delegated to other machines (ADE-121).
//
// A child on another machine lists on that machine; this app's copy of that list is only as fresh as its connection to
// that machine (offline here, a dropped stream, an app restart while it finished). The daemon the user is at follows
// each such child itself (`remote_children`, what `kybern_agent_status` reports) and tells clients every change
// (`delegations.remote_changed`). `state.ts` lays these records over the child set (`withRemoteRecords`), so a launch
// row and a sidebar child say Done/Failed/Stopped within seconds of the child ending, and a child the other machine's
// list lacks still has a row.
//
// Reads (`delegations.remote_list`, per parent) happen when the daemon connects or reconnects (every parent known
// here), and once per connection for a parent that has children at work (`active_children`, which counts remote
// ones), has a listed remote child that still says it works, or is opened. Changes after that arrive as
// notifications. A record whose child appeared or ended asks for a fresh read of its machine's lists.
import { DELEGATIONS_REMOTE_CHANGED_NOTIFICATION, DELEGATIONS_REMOTE_RESYNC_NOTIFICATION, type MachineId, type RemoteChild, type Thread, type ThreadId } from '../../../../packages/kybern-client/src/types.ts'
import { remoteChildStandIn, withRemoteRecord } from '../../../../packages/kybern-client/src/delegations.ts'

export class RemoteDelegationStore {
  private readonly byTask = new Map<string, RemoteChild>()
  /** When each record was last written (`version`), so a read sent before a notification cannot drop its record. */
  private readonly writtenAt = new Map<string, number>()
  private version = 0
  private snapshot: readonly RemoteChild[] = []
  private stale = false
  private readonly listeners = new Set<() => void>()

  /** Every record; the same array until something changes. */
  list(): readonly RemoteChild[] {
    if (this.stale) { this.stale = false; this.snapshot = [...this.byTask.values()] }
    return this.snapshot
  }

  get(taskId: string): RemoteChild | undefined { return this.byTask.get(taskId) }

  /** The parents records are known for, to read again after a reconnect. */
  parents(): ThreadId[] { return [...new Set([...this.byTask.values()].map((record) => record.parent_thread_id))] }

  /** A mark to pass to `replaceParent` for a read sent now. */
  mark(): number { return this.version }

  /** One record (a notification). An older copy (`updated_at`) changes nothing. Returns the record it replaced, or null when unchanged. */
  put(record: RemoteChild): { previous: RemoteChild | undefined } | null {
    const previous = this.byTask.get(record.task_id)
    if (previous && (previous === record || Date.parse(previous.updated_at) > Date.parse(record.updated_at) || same(previous, record))) return null
    this.byTask.set(record.task_id, record)
    this.writtenAt.set(record.task_id, ++this.version)
    this.changed()
    return { previous }
  }

  /**
   * A full read of one parent's records, sent at `since` (`mark()`): they replace what was known for it. A record the
   * daemon no longer has leaves, unless a notification wrote it after the read was sent (the read could not know it).
   */
  replaceParent(parentId: ThreadId, records: readonly RemoteChild[], since = Infinity): RemoteChild[] {
    const moved: RemoteChild[] = []
    let changed = false
    const wanted = new Set(records.map((record) => record.task_id))
    for (const [taskId, record] of this.byTask) {
      if (record.parent_thread_id !== parentId || wanted.has(taskId) || (this.writtenAt.get(taskId) ?? 0) > since) continue
      this.byTask.delete(taskId)
      this.writtenAt.delete(taskId)
      changed = true
    }
    for (const record of records) {
      const current = this.byTask.get(record.task_id)
      if (current && (Date.parse(current.updated_at) > Date.parse(record.updated_at) || same(current, record))) continue
      if (!current || needsListRead(current, record)) moved.push(record)
      this.byTask.set(record.task_id, record)
      this.writtenAt.set(record.task_id, ++this.version)
      changed = true
    }
    if (changed) this.changed()
    return moved
  }

  clear() {
    if (!this.byTask.size) return
    this.byTask.clear()
    this.writtenAt.clear()
    this.changed()
  }

  subscribe(listener: () => void): () => void {
    this.listeners.add(listener)
    return () => { this.listeners.delete(listener) }
  }

  private changed() {
    this.stale = true
    for (const listener of [...this.listeners]) listener()
  }
}

function same(a: RemoteChild, b: RemoteChild): boolean {
  return a.updated_at === b.updated_at && a.state === b.state && a.remote_thread_id === b.remote_thread_id && a.stop_requested === b.stop_requested
}

/** A change the target's own list should show too: the child was made there, or ended. Progress alone is not. */
export function needsListRead(previous: RemoteChild | undefined, next: RemoteChild): boolean {
  if (!previous) return true
  return previous.state !== next.state || previous.remote_thread_id !== next.remote_thread_id
}

type ParentFields = Pick<Thread, 'project_id' | 'permission_mode' | 'cwd'>

/**
 * The child set with the records laid over it (`withRemoteRecords` in the shared client), memoized so an unchanged child
 * or record keeps its object: `nav.children` only changes when a record or a child does.
 */
export class RemoteRecordOverlay {
  private readonly applied = new WeakMap<object, { record: RemoteChild; result: object }>()
  private readonly standIns = new WeakMap<RemoteChild, { parent: string; result: Thread & { machineId: string } }>()

  apply<T extends Thread & { machineId?: string }>(children: readonly T[], records: readonly RemoteChild[], parents: ReadonlyMap<ThreadId, ParentFields>): (T | (Thread & { machineId: string }))[] {
    if (!records.length) return children as T[]
    const byTask = new Map<string, RemoteChild>()
    for (const record of records) byTask.set(record.task_id, record)
    const seen = new Set<string>()
    const out: (T | (Thread & { machineId: string }))[] = children.map((child) => {
      const record = child.delegation ? byTask.get(child.delegation.task_id) : undefined
      if (!record) return child
      seen.add(record.task_id)
      const old = this.applied.get(child)
      if (old?.record === record) return old.result as T
      const result = withRemoteRecord(child, record)
      this.applied.set(child, { record, result })
      return result
    })
    const listed = new Set(children.map((child) => child.id))
    for (const record of byTask.values()) {
      if (seen.has(record.task_id)) continue
      const parent = parents.get(record.parent_thread_id)
      // Not acknowledged yet (no thread to open there), or listed without its delegation (an older copy): no row.
      if (!parent || !record.remote_thread_id || listed.has(record.remote_thread_id)) continue
      // Keyed by the fields it copies, not the parent object, which changes on every event of a working parent.
      const key = `${parent.project_id}|${parent.permission_mode}|${parent.cwd}`
      const old = this.standIns.get(record)
      if (old && old.parent === key) { out.push(old.result); continue }
      const result = remoteChildStandIn(record, parent)
      this.standIns.set(record, { parent: key, result })
      out.push(result)
    }
    return out
  }
}

/** What `RemoteDelegations` needs from the daemon connection and the machine hub. */
export interface RemoteDelegationDeps {
  call: (parentId: ThreadId) => Promise<{ children: RemoteChild[] }>
  onNotification: (method: string, handler: (params: unknown) => void) => () => void
  /** Read one machine's lists again (`MachineHub.refreshMachineLists`). */
  refreshMachine: (machineId: MachineId) => void
}

/** Follows one daemon connection's records into a store. */
export class RemoteDelegations {
  private readonly loads = new Map<ThreadId, Promise<void>>()
  /** Parents read on this connection: notifications keep them current, so `ensure` does not read them again. */
  private readonly read = new Set<ThreadId>()
  private stop: (() => void) | undefined
  private disposed = false
  /** A daemon older than `delegations.remote_list`: nothing to read until it reconnects. */
  private unsupported = false

  constructor(private readonly store: RemoteDelegationStore, private readonly deps: RemoteDelegationDeps) {}

  start() {
    const stops = [
      this.deps.onNotification(DELEGATIONS_REMOTE_CHANGED_NOTIFICATION, (params) => {
        const record = params as RemoteChild
        if (!record?.task_id || !record.parent_thread_id) return
        const put = this.store.put(record)
        if (put && needsListRead(put.previous, record)) this.deps.refreshMachine(record.target)
      }),
      // The daemon dropped changes this connection was too slow to take: what it records is read again.
      this.deps.onNotification(DELEGATIONS_REMOTE_RESYNC_NOTIFICATION, () => { if (!this.disposed) void this.reread() }),
    ]
    this.stop = () => { for (const stop of stops) stop() }
  }

  dispose() {
    this.disposed = true
    this.stop?.()
    this.stop = undefined
    this.loads.clear()
    this.read.clear()
  }

  /** The daemon connected or reconnected: what it records may have changed while this app did not hear it. */
  reread(extra: readonly ThreadId[] = []): Promise<void> {
    this.unsupported = false
    this.read.clear()
    return Promise.all([...new Set([...this.store.parents(), ...extra])].map((id) => this.load(id))).then(() => {})
  }

  /** Read a parent once per connection; later changes arrive as notifications. */
  ensure(parentId: ThreadId): Promise<void> {
    return this.read.has(parentId) ? Promise.resolve() : this.load(parentId)
  }

  /** One parent's records, one read at a time per parent. A failed read keeps what was known. */
  load(parentId: ThreadId): Promise<void> {
    const pending = this.loads.get(parentId)
    if (pending) return pending
    if (this.unsupported || this.disposed) return Promise.resolve()
    this.read.add(parentId)
    const since = this.store.mark()
    const request = this.deps.call(parentId)
      .then((result) => {
        // A connection that was replaced meanwhile says nothing more.
        if (this.disposed) return
        const moved = this.store.replaceParent(parentId, result.children ?? [], since)
        for (const target of new Set(moved.map((record) => record.target))) this.deps.refreshMachine(target)
      })
      .catch((error: unknown) => {
        this.read.delete(parentId)
        if (/-32601|unknown method|method not found/i.test(error instanceof Error ? error.message : String(error))) this.unsupported = true
      })
      .finally(() => this.loads.delete(parentId))
    this.loads.set(parentId, request)
    return request
  }
}

/** The app's store: `nav/live.ts` fills it, `state.ts` lays it over the child set. */
export const remoteDelegationStore = new RemoteDelegationStore()
