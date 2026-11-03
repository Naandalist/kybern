// ADE-121: children on other machines follow the delegating daemon's own record, not this app's copy of their machine's list.
import { expect, test } from 'bun:test'
import { childState, launchState } from '../../../../packages/kybern-client/src/delegations.ts'
import { DELEGATIONS_REMOTE_CHANGED_NOTIFICATION, DELEGATIONS_REMOTE_RESYNC_NOTIFICATION, type RemoteChild, type Thread } from '../../../../packages/kybern-client/src/types.ts'
import { RemoteDelegationStore, RemoteDelegations, RemoteRecordOverlay, needsListRead } from './remoteDelegations.ts'

const PARENT = 'parent-1'
const parent = { id: PARENT, project_id: 'p1', permission_mode: 'full-access', cwd: '/w/p1' } as unknown as Thread
const parents = new Map([[PARENT, parent]])

function record(task: string, state: RemoteChild['state'], updated: string, extra: Partial<RemoteChild> = {}): RemoteChild {
  return {
    task_id: task, operation_id: `op-${task}`, parent_thread_id: PARENT, target: 'satyamas', remote_thread_id: `thread-${task}`, state, last_seq: 1,
    title: `Child ${task}`, provider: 'claude-code', model: 'opus',
    info: { task_id: task, operation_id: `op-${task}`, parent_thread_id: PARENT, depth: 1, role: 'implementation', workspace: 'worktree', status: state === 'completed' ? 'completed' : 'running', started_at: '2026-11-02T15:42:00Z', ...(state === 'completed' ? { completed_at: '2026-11-02T18:06:00Z', result: 'Done it' } : {}) },
    created_at: '2026-11-02T15:42:00Z', updated_at: updated, ...extra,
  }
}

/** The other machine's copy of a child, as this app last listed it: still running. */
function listed(task: string, status = 'running'): Thread & { machineId: string } {
  return {
    id: `thread-${task}`, project_id: 'p1', parent_thread_id: PARENT, title: `Child ${task}`, status, pinned: false, last_seq: 4, provider: { kind: 'claude-code', instance: '' },
    created_at: '2026-11-02T15:42:00Z', updated_at: '2026-11-02T15:45:00Z', cwd: '/w', permission_mode: 'full-access',
    delegation: { task_id: task, operation_id: `op-${task}`, parent_thread_id: PARENT, depth: 1, role: 'implementation', workspace: 'worktree', status: status === 'running' ? 'running' : 'completed', started_at: '2026-11-02T15:42:00Z' },
    origin: { machine_id: 'mac', machine_name: 'Mac', thread_id: PARENT, operation_id: `op-${task}`, kind: 'delegation', permission_cap: 'full-access' },
    machineId: 'satyamas',
  } as unknown as Thread & { machineId: string }
}

function follower() {
  const store = new RemoteDelegationStore()
  const handlers = new Map<string, (params: unknown) => void>()
  const refreshed: string[] = []
  let answer: RemoteChild[] = []
  const calls: string[] = []
  const delegations = new RemoteDelegations(store, {
    call: async (parentId) => { calls.push(parentId); return { children: answer } },
    onNotification: (method, handler) => { handlers.set(method, handler); return () => handlers.delete(method) },
    refreshMachine: (id) => { refreshed.push(id) },
  })
  delegations.start()
  return { store, delegations, refreshed, calls, handlers, notify: (params: RemoteChild) => handlers.get(DELEGATIONS_REMOTE_CHANGED_NOTIFICATION)?.(params), answer: (next: RemoteChild[]) => { answer = next } }
}

test('a launch row of an unknown remote child whose record is completed says Done, not Working', () => {
  const call = { complete: true, isError: false }
  expect(launchState(undefined, call)).toEqual({ phase: 'working', word: 'Working' })
  expect(launchState(undefined, call, record('a', 'completed', '2026-11-02T18:06:00Z'))).toEqual({ phase: 'done', word: 'Done' })
  expect(launchState(undefined, call, record('a', 'cancelled', '2026-11-02T18:06:00Z'))).toEqual({ phase: 'stopped', word: 'Stopped' })
  expect(launchState(undefined, call, record('a', 'unreachable', '2026-11-02T18:06:00Z')).phase).toBe('failed')
})

test('a stale copy that still says Working gives way to the record; a copy that ended stays as it is', () => {
  const overlay = new RemoteRecordOverlay()
  const stale = listed('a')
  const [shown] = overlay.apply([stale], [record('a', 'completed', '2026-11-02T18:06:00Z')], parents)
  expect(childState(shown!)).toEqual({ phase: 'done', word: 'Done' })
  expect(shown!.status).toBe('idle')
  expect(shown!.delegation?.result).toBe('Done it')
  const ended = listed('a', 'idle')
  expect(overlay.apply([ended], [record('a', 'running', '2026-11-02T16:00:00Z')], parents)[0]).toBe(ended)
  // Woken again by a message after it ended (its copy changed after the end): the record does not follow that. Both
  // times are the other machine's only when the record's end came from following the child to its end.
  const woken = { ...listed('a'), updated_at: '2026-11-02T19:00:00Z' }
  expect(overlay.apply([woken], [record('a', 'completed', '2026-11-02T18:06:00Z', { completed_at_from_target: true })], parents)[0]).toBe(woken)
  // An end this machine recorded by its own clock cannot be compared with the other machine's: the settled record wins.
  expect(childState(overlay.apply([woken], [record('a', 'completed', '2026-11-02T18:06:00Z')], parents)[0]!)).toEqual({ phase: 'done', word: 'Done' })
})

test('the overlay keeps object identity while nothing changes', () => {
  const overlay = new RemoteRecordOverlay()
  const stale = listed('a')
  const records = [record('a', 'completed', '2026-11-02T18:06:00Z')]
  const first = overlay.apply([stale], records, parents)
  expect(overlay.apply([stale], records, parents)[0]).toBe(first[0]!)
})

test('a child the other machine list lacks gets a row under its parent, on its machine', () => {
  const overlay = new RemoteRecordOverlay()
  const rows = overlay.apply([], [record('b', 'running', '2026-11-02T19:18:00Z')], parents)
  expect(rows).toHaveLength(1)
  expect(rows[0]).toMatchObject({ id: 'thread-b', parent_thread_id: PARENT, project_id: 'p1', machineId: 'satyamas', title: 'Child b', status: 'running' })
  expect(childState(rows[0]!).word).toBe('Working')
  // Its machine's list arrives: the listed row replaces the stand-in.
  expect(overlay.apply([listed('b')], [record('b', 'running', '2026-11-02T19:18:00Z')], parents).map((row) => row.id)).toEqual(['thread-b'])
  // No parent listed here: nothing to hang it under.
  expect(overlay.apply([], [record('c', 'running', '2026-11-02T19:18:00Z', { parent_thread_id: 'elsewhere' })], parents)).toEqual([])
})

// The user's case: child A followed while its machine was reachable, the machine went offline here and came back, A finished and
// child B was delegated meanwhile. The sidebar shows B, A goes done, and each change asks for that machine's lists again.
test('a child followed through an offline spell: B appears, A goes done, the machine is read again', async () => {
  const { store, delegations, refreshed, notify, answer, calls } = follower()
  answer([record('a', 'running', '2026-11-02T15:45:00Z')])
  await delegations.load(PARENT)
  const overlay = new RemoteRecordOverlay()
  const frozen = [listed('a')]
  expect(overlay.apply(frozen, store.list(), parents).map((row) => [row.id, childState(row).word])).toEqual([['thread-a', 'Working']])

  // Live: B is acknowledged on the other machine, A ends.
  notify(record('b', 'running', '2026-11-02T19:18:00Z'))
  notify(record('a', 'completed', '2026-11-02T18:06:00Z'))
  expect(overlay.apply(frozen, store.list(), parents).map((row) => [row.id, childState(row).word])).toEqual([['thread-a', 'Done'], ['thread-b', 'Working']])
  expect(refreshed).toEqual(['satyamas', 'satyamas', 'satyamas'])
  // Progress of a running child alone does not read the machine's lists again.
  notify(record('b', 'running', '2026-11-02T19:20:00Z', { last_seq: 9 }))
  expect(refreshed).toHaveLength(3)
  // An older copy changes nothing.
  notify(record('a', 'running', '2026-11-02T16:00:00Z'))
  expect(store.get('a')?.state).toBe('completed')

  // The app's daemon reconnects (or the app restarts) after B ended while nobody listened: a fresh read settles it.
  answer([record('a', 'completed', '2026-11-02T18:06:00Z'), record('b', 'completed', '2026-11-02T20:44:00Z')])
  calls.length = 0
  await delegations.reread()
  expect(calls).toEqual([PARENT])
  expect(overlay.apply(frozen, store.list(), parents).map((row) => [row.id, childState(row).word])).toEqual([['thread-a', 'Done'], ['thread-b', 'Done']])
})

test('a read replaces what a parent had: a record the daemon dropped leaves', async () => {
  const { store, delegations, answer } = follower()
  answer([record('a', 'running', '2026-11-02T15:45:00Z'), record('b', 'running', '2026-11-02T15:45:00Z')])
  await delegations.load(PARENT)
  answer([record('b', 'running', '2026-11-02T15:45:00Z')])
  await delegations.load(PARENT)
  expect(store.list().map((r) => r.task_id)).toEqual(['b'])
})

test('a daemon without the method is asked once', async () => {
  const store = new RemoteDelegationStore()
  let calls = 0
  const delegations = new RemoteDelegations(store, {
    call: async () => { calls++; throw new Error('Method not found: delegations.remote_list') },
    onNotification: () => () => {},
    refreshMachine: () => {},
  })
  await delegations.load(PARENT)
  await delegations.load(PARENT)
  expect(calls).toBe(1)
})

test('only a new child or an ended one needs its machine read again', () => {
  const running = record('a', 'running', '2026-11-02T15:45:00Z')
  expect(needsListRead(undefined, running)).toBe(true)
  expect(needsListRead(running, { ...running, last_seq: 7 })).toBe(false)
  expect(needsListRead(running, record('a', 'completed', '2026-11-02T18:06:00Z'))).toBe(true)
  expect(needsListRead({ ...running, remote_thread_id: null }, running)).toBe(true)
})

test('a record a notification wrote while a read was on its way is not dropped by that read', async () => {
  const store = new RemoteDelegationStore()
  let answer: (value: { children: RemoteChild[] }) => void = () => {}
  let notify: (params: unknown) => void = () => {}
  const delegations = new RemoteDelegations(store, {
    call: () => new Promise((resolve) => { answer = resolve }),
    onNotification: (method, handler) => { if (method === DELEGATIONS_REMOTE_CHANGED_NOTIFICATION) notify = handler; return () => {} },
    refreshMachine: () => {},
  })
  delegations.start()
  const reading = delegations.load(PARENT)
  notify(record('b', 'running', '2026-11-02T19:18:00Z'))
  answer({ children: [record('a', 'running', '2026-11-02T15:45:00Z')] })
  await reading
  expect(store.list().map((r) => r.task_id).sort()).toEqual(['a', 'b'])
})

test('a parent is read once per connection; a reconnect reads it again', async () => {
  const { delegations, calls, answer } = follower()
  answer([record('a', 'running', '2026-11-02T15:45:00Z')])
  await delegations.ensure(PARENT)
  await delegations.ensure(PARENT)
  expect(calls).toEqual([PARENT])
  await delegations.reread()
  expect(calls).toEqual([PARENT, PARENT])
})

test('a child not yet acknowledged on its machine gets no row to open', () => {
  const overlay = new RemoteRecordOverlay()
  expect(overlay.apply([], [record('p', 'pending', '2026-11-02T15:42:00Z', { remote_thread_id: null })], parents)).toEqual([])
})

// A connection too slow for the daemon's notifications misses records; the daemon says so and they are read again.
test('a resync hint reads every known parent again', async () => {
  const { store, delegations, calls, handlers, answer } = follower()
  answer([record('a', 'running', '2026-11-02T16:00:00Z')])
  await delegations.load(PARENT)
  expect(calls).toEqual([PARENT])
  answer([record('a', 'completed', '2026-11-02T18:06:00Z')])
  handlers.get(DELEGATIONS_REMOTE_RESYNC_NOTIFICATION)?.({})
  await new Promise((resolve) => setTimeout(resolve, 0))
  expect(calls).toEqual([PARENT, PARENT])
  expect(store.list()[0]?.state).toBe('completed')
  delegations.dispose()
  expect(handlers.size).toBe(0)
})
