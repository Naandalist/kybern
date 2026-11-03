// Threads of another machine follow that machine's events like local ones do: fake daemons behind a real hub.
import { afterEach, expect, test } from 'bun:test'
import type { KybernClient } from '../../../../packages/kybern-client/src/client.ts'
import { scopedKey } from '../../../../packages/kybern-client/src/machineMerge.ts'
import type { Project, Thread } from '../../../../packages/kybern-client/src/types.ts'
import type { EnvironmentProfile } from '../environments/registry.ts'
import { MachineHub } from '../machines/hub.ts'
import { recordsFromRegistry } from '../machines/records.ts'
import { RemoteLive, activityMaps, withRemote } from './remoteLive.ts'
import { RemoteChildStore } from './remoteChildren.ts'

const project = (id: string): Project => ({ id, name: id, path: `/w/${id}`, is_git: true, brief_in_all_threads: false, repo: { root_commit: 'root' }, created_at: '2026-10-01T00:00:00Z', updated_at: '2026-10-01T00:00:00Z' }) as unknown as Project
const thread = (id: string, status: string, last_seq: number, extra: Record<string, unknown> = {}): Thread => ({ id, project_id: 'p1', title: id, status, pinned: false, last_seq, updated_at: '2026-10-02T00:00:00Z', ...extra }) as unknown as Thread

class Daemon {
  status = 'idle'
  info: unknown
  endpoint = { url: 'ws://fake/ws', token: 'tok' }
  streams: { handler: (event: unknown) => void; open: boolean; onSubscribed?: (head: number, replay: { resumed: boolean; supported: boolean }) => void }[] = []
  lists: { threads: Thread[]; activity?: unknown[] } = { threads: [] }
  /** Children by parent, as `threads.list { parent_thread_id }` answers. */
  children: Record<string, Thread[]> = {}
  childReads: string[] = []
  reads = 0
  private statuses = new Set<(status: string) => void>()
  constructor(readonly machineId: string) { this.info = { environment_id: machineId, machine_id: machineId, version: '0.19.0', features: [] } }
  setStatus(status: string) { this.status = status; for (const fn of [...this.statuses]) fn(status) }
  connect() { queueMicrotask(() => this.setStatus('open')) }
  close() { this.status = 'closed' }
  onStatus(fn: (status: string) => void) { this.statuses.add(fn); return () => { this.statuses.delete(fn) } }
  onNotification() { return () => {} }
  async checkConnection() {}
  async call(method: string, params?: { parent_thread_id?: string }) {
    if (method === 'projects.list') return { projects: [project('p1')] }
    if (method === 'threads.list' && params?.parent_thread_id) { this.childReads.push(params.parent_thread_id); return { threads: this.children[params.parent_thread_id] ?? [] } }
    if (method === 'threads.list') { this.reads++; return { threads: this.lists.threads, activity: this.lists.activity } }
    throw new Error(`no handler for ${method}`)
  }
  callRaw(method: string, params?: { parent_thread_id?: string }) { return this.call(method, params) }
  subscribeEvents(_: unknown, handler: (event: unknown) => void, onSubscribed?: (head: number, replay: { resumed: boolean; supported: boolean }) => void) {
    const stream = { handler, open: true, onSubscribed }
    this.streams.push(stream)
    return { unsubscribe() { stream.open = false }, lastSeq: undefined }
  }
  emit(event: Record<string, unknown>) { for (const stream of this.streams) if (stream.open) stream.handler(event) }
  asClient() { return this as unknown as KybernClient }
}

const settle = async () => { for (let i = 0; i < 20; i++) await new Promise((resolve) => setImmediate(resolve)) }
const local: EnvironmentProfile = { id: 'local', name: 'This Mac', local: true }
const vpsProfile: EnvironmentProfile = { id: 'vps', name: 'vps', local: false, url: 'wss://vps.example/ws', environment_id: 'vps-1' }

let hubs: MachineHub[] = []
let lives: RemoteLive[] = []
afterEach(() => { for (const live of lives) live.stop(); for (const hub of hubs) hub.dispose(); hubs = []; lives = [] })

async function setup(options: { remote?: boolean; selected?: { id: string; machineId?: string } } = {}) {
  const mac = new Daemon('mac-1')
  const vps = new Daemon('vps-1')
  vps.lists.threads = [thread('r1', 'running', 5), thread('shared', 'idle', 1)]
  const timers: (() => void)[] = []
  const hub = new MachineHub({
    records: () => recordsFromRegistry({ selected_id: 'local', environments: options.remote === false ? [] : [vpsProfile] }, local),
    readCredential: async (id) => `tok-${id}`,
    setTimer: (fn) => { timers.push(fn); return timers.length },
    clearTimer: () => {},
    transport: { createClient: () => vps as never },
  })
  hubs.push(hub)
  hub.beginPrimary(mac.asClient(), { id: 'local', name: 'This Mac' })
  mac.setStatus('open')
  const adopted = hub.primaryOpened(mac.asClient())
  hub.ingestPrimary(mac.asClient(), [project('p1')], [thread('shared', 'idle', 1)])
  await adopted
  await hub.connectOthers()
  await settle()
  let unread: Record<string, true> = {}
  let published = 0
  const children = new RemoteChildStore()
  const live = new RemoteLive({
    hub,
    children,
    state: () => {
      const remoteThreads = hub.view().threads.all.filter((t) => t.machineId === 'vps-1').map((t) => ({ id: t.thread.id, machineId: 'vps-1' }))
      return { threads: [{ id: 'shared' }, ...remoteThreads], selectedId: options.selected?.id ?? null, ...(options.selected?.machineId ? { selectedMachineId: options.selected.machineId } : {}) }
    },
    unread: () => unread,
    setUnread: (next) => { unread = next },
    publish: () => { published++ },
    setTimer: (fn) => { timers.push(fn); return timers.length },
    clearTimer: (handle) => { timers[(handle as number) - 1] = () => {} },
  })
  lives.push(live)
  live.start()
  const statusOf = (id: string) => hub.view().threads.byKey.get(scopedKey('vps-1', id))?.thread.status
  return { mac, vps, hub, live, children, statusOf, unread: () => unread, published: () => published, runTimers: () => timers.splice(0).forEach((fn) => fn()) }
}

test('a remote thread that finished its turn stops showing as working', async () => {
  const { vps, statusOf } = await setup()
  expect(statusOf('r1')).toBe('running')
  vps.emit({ kind: 'thread_updated', seq: 9, thread: thread('r1', 'idle', 9) })
  expect(statusOf('r1')).toBe('idle')
})

test('an older update does not bring back a finished status, and an archived thread leaves the list', async () => {
  const { vps, statusOf, hub } = await setup()
  vps.emit({ kind: 'thread_updated', seq: 9, thread: thread('r1', 'idle', 9) })
  vps.emit({ kind: 'thread_updated', seq: 6, thread: thread('r1', 'running', 6) })
  expect(statusOf('r1')).toBe('idle')
  vps.emit({ kind: 'thread_archived', seq: 10, thread_id: 'r1' })
  expect(hub.view().threads.byKey.has(scopedKey('vps-1', 'r1'))).toBe(false)
})

test('a new thread of the other machine is listed when it is created', async () => {
  const { vps, statusOf } = await setup()
  vps.emit({ kind: 'thread_created', seq: 3, thread: thread('fresh', 'running', 3) })
  expect(statusOf('fresh')).toBe('running')
})

test('a remote update never touches a local thread of the same id', async () => {
  const { vps, hub } = await setup()
  vps.emit({ kind: 'thread_updated', seq: 9, thread: thread('shared', 'running', 9) })
  expect(hub.view().threads.byKey.get(scopedKey('mac-1', 'shared'))?.thread.status).toBe('idle')
  expect(hub.view().threads.byKey.get(scopedKey('vps-1', 'shared'))?.thread.status).toBe('running')
})

test('a remote turn that completes while another thread is open marks it unread', async () => {
  const { vps, unread } = await setup({ selected: { id: 'other' } })
  vps.emit({ kind: 'turn_completed', seq: 9, thread_id: 'r1', stop_reason: 'completed' })
  expect(unread()).toEqual({ r1: true })
})

test('a remote turn that completes in the open thread, or stops for another reason, does not', async () => {
  const open = await setup({ selected: { id: 'r1', machineId: 'vps-1' } })
  open.vps.emit({ kind: 'turn_completed', seq: 9, thread_id: 'r1', stop_reason: 'completed' })
  expect(open.unread()).toEqual({})
  const failed = await setup()
  failed.vps.emit({ kind: 'turn_completed', seq: 9, thread_id: 'r1', stop_reason: 'interrupted' })
  expect(failed.unread()).toEqual({})
})

test('a remote approval reads that machine again and shows its pending badge and activity', async () => {
  const { vps, live, runTimers, published } = await setup()
  const reads = vps.reads
  vps.lists.threads = [thread('r1', 'awaiting-approval', 9)]
  vps.lists.activity = [{ thread_id: 'r1', state: 'working', active_agents: 0, active_processes: 0, active_monitors: 0, pending_request: 'approval' }]
  vps.emit({ kind: 'approval_requested', seq: 9, thread_id: 'r1' })
  vps.emit({ kind: 'approval_requested', seq: 10, thread_id: 'r1' })
  runTimers()
  await settle()
  expect(vps.reads - reads).toBe(1)
  expect(live.pending).toEqual({ r1: { own: 'approval' } })
  expect(live.activity).toEqual({ r1: 'working' })
  expect(published()).toBeGreaterThan(0)
  // Answered: the next read clears both.
  vps.lists.activity = []
  vps.emit({ kind: 'approval_resolved', seq: 11, thread_id: 'r1' })
  runTimers()
  await settle()
  expect(live.pending).toEqual({})
  expect(live.activity).toEqual({})
})

test('the machine the user is at is not followed here, and one machine opens no stream', async () => {
  const { mac } = await setup()
  mac.emit({ kind: 'thread_updated', seq: 1, thread: thread('shared', 'running', 9) })
  expect(mac.streams).toEqual([])
  const single = await setup({ remote: false })
  expect(single.mac.streams).toEqual([])
  expect(single.live.activity).toEqual({})
  expect(single.hub.machineCount).toBe(1)
})

test('activity maps and the merge with local entries', () => {
  const maps = activityMaps([
    { thread_id: 'a', state: 'monitoring', active_agents: 0, active_processes: 0, active_monitors: 1 },
    { thread_id: 'b', state: null, active_agents: 0, active_processes: 0, active_monitors: 0, pending_child_request: 'question' },
  ])
  expect(maps.activity).toEqual({ a: 'monitoring' })
  expect(maps.pending).toEqual({ b: { child: 'question' } })
  const local = { a: 'working' } as never
  expect(withRemote(local, {})).toBe(local)
  expect(withRemote(local, { a: 'monitoring', c: 'working' } as never)).toEqual({ a: 'working', c: 'working' })
})

// ADE-107: the children of a thread on another machine live on that machine, which leaves them out of its `threads.list`.
const delegated = (id: string, last_seq: number, extra: Record<string, unknown> = {}) =>
  thread(id, 'running', last_seq, { parent_thread_id: 'r1', delegation: { status: 'running', started_at: '2026-10-02T00:00:00Z' }, ...extra })
const childIds = (children: RemoteChildStore, machineId = 'vps-1') => (children.get().get(machineId) ?? []).map((t) => [t.id, t.status])

test('an agent started by a remote thread reads that thread’s children from its own machine', async () => {
  const { mac, vps, children } = await setup()
  vps.children.r1 = [delegated('c1', 3), delegated('c2', 4)]
  vps.emit({ kind: 'runtime_task_started', seq: 5, thread_id: 'r1', task: { id: 'task-1', kind: 'agent' } })
  await settle()
  expect(vps.childReads).toEqual(['r1'])
  expect(mac.childReads).toEqual([])
  expect(childIds(children)).toEqual([['c1', 'running'], ['c2', 'running']])
})

test('a remote thread with children at work reads them when its machine’s lists arrive', async () => {
  const { vps, children, runTimers } = await setup()
  vps.children.r1 = [delegated('c1', 3)]
  vps.lists.threads = [thread('r1', 'running', 5)]
  vps.lists.activity = [{ thread_id: 'r1', state: 'working', active_agents: 0, active_processes: 0, active_monitors: 0, active_children: 1 }]
  vps.emit({ kind: 'approval_requested', seq: 6, thread_id: 'r1' })
  runTimers()
  await settle()
  expect(childIds(children)).toEqual([['c1', 'running']])
})

test('remote children follow their machine’s events, survive its list reads, and leave when archived or when the machine goes', async () => {
  const { vps, hub, live, children, runTimers } = await setup()
  vps.children.r1 = [delegated('c1', 3)]
  await live.loadChildren('vps-1', 'r1')
  vps.emit({ kind: 'thread_updated', seq: 7, thread: delegated('c1', 7, { status: 'idle' }) })
  vps.emit({ kind: 'thread_created', seq: 8, thread: delegated('c2', 8) })
  expect(childIds(children)).toEqual([['c1', 'idle'], ['c2', 'running']])
  // The machine's next list read leaves its children out; they stay.
  vps.emit({ kind: 'runtime_task_updated', seq: 9, thread_id: 'r1', task: { id: 'task-1', kind: 'process' } })
  runTimers()
  await hub.refreshMachineLists('vps-1')
  await settle()
  expect(childIds(children)).toEqual([['c1', 'idle'], ['c2', 'running']])
  // An older update changes nothing; an archived child leaves and a late update does not bring it back.
  vps.emit({ kind: 'thread_updated', seq: 4, thread: delegated('c1', 4) })
  vps.emit({ kind: 'thread_archived', seq: 10, thread_id: 'c2' })
  vps.emit({ kind: 'thread_updated', seq: 11, thread: delegated('c2', 11) })
  expect(childIds(children)).toEqual([['c1', 'idle']])
  children.dropMachine('vps-1')
  expect(children.get().size).toBe(0)
})

test('with one machine no child is read', async () => {
  const single = await setup({ remote: false })
  expect(await single.live.loadChildren('mac-1', 'shared')).toBeUndefined()
  expect(single.children.get().size).toBe(0)
})

// ADE-121: a stream that starts again without replaying what it missed cannot patch the lists up; they are read again.
test('a machine stream that restarts without a replay reads that machine again; one that resumes does not', async () => {
  const { vps } = await setup()
  const stream = vps.streams.find((s) => s.open)!
  const before = vps.reads
  stream.onSubscribed?.(40, { resumed: true, supported: true })
  await settle()
  expect(vps.reads).toBe(before)
  vps.lists.threads = [thread('r1', 'idle', 30), thread('child-b', 'running', 31)]
  stream.onSubscribed?.(40, { resumed: false, supported: true })
  await settle()
  expect(vps.reads).toBe(before + 1)
})
