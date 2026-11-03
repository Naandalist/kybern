// The machine hub with fake daemons: one machine behaves as before, several are
// merged and routed by machine. No sockets, no daemon.
import { afterEach, expect, test } from 'bun:test'
import type { KybernClient } from '../../../../packages/kybern-client/src/client.ts'
import { OfflineError, type MachineTransport } from '../../../../packages/kybern-client/src/machines.ts'
import type { Project, Thread } from '../../../../packages/kybern-client/src/types.ts'
import type { EnvironmentProfile } from '../environments/registry.ts'
import { MachineHub, type MachineHubOptions } from './hub.ts'
import { LEGACY_PREFIX, recordOfProfile, recordsFromRegistry } from './records.ts'
import { finishMachineSetup } from '../composer/machineSetup.ts'

const project = (id: string, name: string, root: string): Project => ({ id, name, path: `/w/${name}`, is_git: true, brief_in_all_threads: false, repo: { root_commit: root }, created_at: '2026-10-01T00:00:00Z', updated_at: '2026-10-01T00:00:00Z' }) as unknown as Project
const thread = (id: string, project_id: string, updated_at: string, extra: Record<string, unknown> = {}): Thread => ({ id, project_id, title: id, status: 'idle', pinned: false, updated_at, ...extra }) as unknown as Thread

type Handler = (params: unknown) => unknown
/** A daemon connection: `KybernClient`'s surface, answering from `handlers`. */
class FakeDaemon {
  status = 'idle'
  info: unknown = null
  endpoint = { url: 'ws://fake/ws', token: 'tok' }
  calls: string[] = []
  closed = 0
  online = true
  private statuses = new Set<(status: string, detail?: string) => void>()
  private notifications = new Map<string, Set<(params: unknown) => void>>()
  constructor(readonly machineId: string, readonly handlers: Record<string, Handler> = {}, features: string[] = []) {
    this.info = { environment_id: machineId, machine_id: machineId, version: '0.19.0', features }
  }
  setStatus(status: string) { this.status = status; for (const fn of [...this.statuses]) fn(status) }
  connect() { queueMicrotask(() => { if (this.online) this.setStatus('open') }) }
  close() { this.closed++; this.status = 'closed' }
  onStatus(fn: (status: string, detail?: string) => void) { this.statuses.add(fn); return () => { this.statuses.delete(fn) } }
  onNotification(method: string, fn: (params: unknown) => void) {
    const set = this.notifications.get(method) ?? new Set()
    set.add(fn); this.notifications.set(method, set)
    return () => { set.delete(fn) }
  }
  notify(method: string) { for (const fn of this.notifications.get(method) ?? []) fn({}) }
  async checkConnection() {}
  async call(method: string, params: unknown) {
    this.calls.push(method)
    const handler = this.handlers[method]
    if (!handler) throw new Error(`no handler for ${method}`)
    return handler(params)
  }
  callRaw(method: string, params: unknown) { return this.call(method, params) }
  /** Event streams opened on this connection (`subscribeEvents`), with what each was asked for. */
  streams: { params: { after_seq?: number }; handler: (event: unknown) => void; open: boolean; onSubscribed?: (head: number, replay: { resumed: boolean; supported: boolean }) => void }[] = []
  subscribeEvents(params: { after_seq?: number }, handler: (event: unknown) => void, onSubscribed?: (head: number, replay: { resumed: boolean; supported: boolean }) => void) {
    const stream = { params, handler, open: true, onSubscribed }
    this.streams.push(stream)
    return { unsubscribe() { stream.open = false }, lastSeq: undefined }
  }
  /** Deliver a thread event to every open stream. */
  emit(event: Record<string, unknown>) { for (const stream of this.streams) if (stream.open) stream.handler(event) }
  asClient() { return this as unknown as KybernClient }
}

const settle = async () => { for (let i = 0; i < 20; i++) await new Promise(resolve => setImmediate(resolve)) }
const local: EnvironmentProfile = { id: 'local', name: 'This Mac', local: true }
const saved = (id: string, extra: Partial<EnvironmentProfile> = {}): EnvironmentProfile => ({ id, name: id, local: false, url: `wss://${id}.example/ws`, ...extra })

let hubs: MachineHub[] = []
function makeHub(profiles: EnvironmentProfile[] = [], options: Partial<MachineHubOptions> = {}) {
  const timers: (() => void)[] = []
  const hub = new MachineHub({
    records: () => recordsFromRegistry({ selected_id: 'local', environments: profiles }, local),
    readCredential: async id => `tok-${id}`,
    setTimer: fn => { timers.push(fn); return timers.length },
    clearTimer: () => {},
    ...options,
  })
  hubs.push(hub)
  return { hub, runTimers: () => timers.splice(0).forEach(fn => fn()) }
}
afterEach(() => { for (const hub of hubs) hub.dispose(); hubs = [] })

/** state.ts's sequence for the primary connection: client built, daemon answered, lists read. */
async function openPrimary(hub: MachineHub, daemon: FakeDaemon, profile = { id: 'local', name: 'This Mac' }, lists?: { projects: Project[]; threads: Thread[] }) {
  hub.beginPrimary(daemon.asClient(), profile)
  daemon.setStatus('open')
  const adopted = hub.primaryOpened(daemon.asClient())
  if (lists) hub.ingestPrimary(daemon.asClient(), lists.projects, lists.threads)
  await adopted
  await settle()
}

test('saved environments become machine records with their profile id, the local daemon first', () => {
  const records = recordsFromRegistry({ selected_id: 'local', environments: [saved('vps', { environment_id: 'env-vps' }), saved('box', { ssh: { target: 'me@box', remote_port: 4173, local_port: 5555 } })] }, local)
  expect(records.map(r => [r.legacyProfileId, r.machineId, r.member])).toEqual([
    ['local', `${LEGACY_PREFIX}local`, false],
    ['vps', 'env-vps', false],
    ['box', `${LEGACY_PREFIX}box`, false],
  ])
  expect(records[1]!.routes).toEqual([{ kind: 'direct_advertised', url: 'https://vps.example' }])
  expect(records[2]!.routes).toEqual([{ kind: 'ssh', url: 'http://127.0.0.1:5555' }])
  expect(recordOfProfile(local).routes).toEqual([])
})

test('one machine: the daemon joins the set under its own id, replacing the stand-in, with no extra daemon calls', async () => {
  const { hub } = makeHub()
  const daemon = new FakeDaemon('mac-1', { 'projects.list': () => { throw new Error('the set must not read lists the app already has') } })
  await openPrimary(hub, daemon, { id: 'local', name: 'This Mac' }, { projects: [project('p1', 'kybern', 'r1')], threads: [thread('t1', 'p1', '2026-10-02T00:00:00Z')] })
  expect(hub.connections().map(c => [c.record.machineId, c.record.legacyProfileId, c.status])).toEqual([['mac-1', 'local', 'needs-update']])
  expect(hub.localMachineId).toBe('mac-1')
  expect(daemon.calls).toEqual([])
  expect(daemon.closed).toBe(0)
  expect(hub.clientFor('mac-1')).toBeDefined()
  // A thread nobody lists yet (a new chat) belongs to the machine the user is at.
  expect(hub.machineOfThread('new-chat')).toBe('mac-1')
  expect(hub.clientForThread('new-chat')).toBe(hub.clientFor('mac-1'))
})

test('one machine: no glyphs, and later list reads wait (by reference) until a reader exists', async () => {
  const { hub, runTimers } = makeHub()
  const daemon = new FakeDaemon('mac-1')
  await openPrimary(hub, daemon, undefined, { projects: [project('p1', 'kybern', 'r1')], threads: [thread('t1', 'p1', '2026-10-02T00:00:00Z')] })
  // Joining read the app's first lists once.
  const first = hub.view()
  expect(first.showGlyphs).toBe(false)
  expect(first.machines.map(m => [m.machineId, m.showGlyph, m.isLocal])).toEqual([['mac-1', false, true]])
  expect(first.threads.all.map(t => [t.machineId, t.thread.id])).toEqual([['mac-1', 't1']])
  // A refresh with nobody watching the merged view costs the set nothing.
  hub.ingestPrimary(daemon.asClient(), [project('p1', 'kybern', 'r1')], [thread('t1', 'p1', '2026-10-02T00:00:00Z'), thread('t2', 'p1', '2026-10-03T00:00:00Z')])
  runTimers()
  expect(hub.view()).toBe(first)
  const seen: number[] = []
  const off = hub.subscribe(() => seen.push(hub.view().threads.all.length))
  runTimers()
  const view = hub.view()
  expect(view.threads.all.map(t => t.thread.id)).toEqual(['t2', 't1'])
  expect(view.showGlyphs).toBe(false)
  expect(hub.view()).toBe(view)
  expect(seen.at(-1)).toBe(2)
  off()
})

test('the primary joining or leaving never closes or reconnects the app’s own client', async () => {
  const { hub } = makeHub()
  const daemon = new FakeDaemon('mac-1')
  await openPrimary(hub, daemon)
  hub.releasePrimary()
  daemon.close()
  await settle()
  expect(hub.set.connection('mac-1')?.status).toBe('paused')
  expect(() => hub.clientFor('mac-1')).toThrow(OfflineError)
  // Switching back to the same daemon with a fresh client resumes the same machine.
  const again = new FakeDaemon('mac-1')
  await openPrimary(hub, again)
  expect(hub.set.connection('mac-1')?.status).toBe('needs-update')
  expect(hub.connections()).toHaveLength(1)
  expect(daemon.closed).toBe(1)
  expect(again.closed).toBe(0)
})

test('a saved machine connects once the app’s own connection is up, and takes the daemon’s id when it answers', async () => {
  const created: string[] = []
  const vps = new FakeDaemon('vps-1', { 'projects.list': () => ({ projects: [] }), 'threads.list': () => ({ threads: [] }) })
  const dropped: string[] = []
  const { hub } = makeHub([saved('vps')], { forgetProfile: id => { dropped.push(id) }, transport: { createClient: ({ record }) => { created.push(record.machineId); return vps as never } } })
  await openPrimary(hub, new FakeDaemon('mac-1'))
  await hub.connectOthers()
  await settle()
  expect(created).toEqual([`${LEGACY_PREFIX}vps`])
  expect(hub.connections().map(c => [c.record.machineId, c.status])).toEqual([['mac-1', 'needs-update'], ['vps-1', 'needs-update']])
  expect(hub.connections()[1]!.record.legacyProfileId).toBe('vps')
  expect(dropped).toEqual(['vps'])
})

test('a daemon with the network on is a member and teaches the set the rest of the network', async () => {
  const card = (id: string, name: string) => ({ machine_id: id, name, os: 'linux', status: 'online' })
  const mac = new FakeDaemon('mac-1', {
    'network.status': () => ({ enabled: true, self_machine: card('mac-1', 'Mac'), members: [card('mac-1', 'Mac'), card('vps-1', 'VPS')], found: [], outbox: {}, tailscale: {} }),
    'network.routes': () => ({ routes: [{ kind: 'direct_tailnet', url: 'http://vps:4199' }] }),
    'network.client_grant': () => ({ token: 'minted', scopes: [], routes: [] }),
  }, ['network.v1'])
  const created: string[] = []
  const vps = new FakeDaemon('vps-1', { 'projects.list': () => ({ projects: [] }), 'threads.list': () => ({ threads: [] }) }, ['network.v1'])
  const { hub } = makeHub([], { transport: { probe: async () => { throw new Error('no probe in this test') }, createClient: ({ record }) => { created.push(record.machineId); return vps as never } } })
  await openPrimary(hub, mac)
  expect(hub.set.connection('mac-1')?.record.member).toBe(true)
  expect(hub.connections().map(c => c.record.machineId)).toContain('vps-1')
  expect(mac.calls).toContain('network.status')
})

test('a daemon with the network off is not a member', async () => {
  const mac = new FakeDaemon('mac-1', { 'network.status': () => ({ enabled: false, self_machine: {}, members: [], found: [], outbox: {}, tailscale: {} }) }, ['network.v1'])
  const { hub } = makeHub()
  await openPrimary(hub, mac)
  expect(hub.set.connection('mac-1')?.record.member).toBe(false)
})

test('several machines: lists merge, rows carry their machine, and clients are routed by thread and project', async () => {
  const vps = new FakeDaemon('vps-1', {
    'projects.list': () => ({ projects: [project('p9', 'kybern', 'r1'), project('p10', 'site', 'r2')] }),
    'threads.list': () => ({ threads: [thread('t-v', 'p9', '2026-10-03T00:00:00Z'), thread('t-same', 'p9', '2026-10-01T00:00:00Z')] }),
  })
  const { hub, runTimers } = makeHub([saved('vps', { environment_id: 'vps-1' })], { transport: { createClient: () => vps as never } })
  const mac = new FakeDaemon('mac-1')
  await openPrimary(hub, mac, undefined, { projects: [project('p1', 'kybern', 'r1')], threads: [thread('t-m', 'p1', '2026-10-02T00:00:00Z'), thread('t-same', 'p1', '2026-10-04T00:00:00Z')] })
  await hub.set.ensureAll()
  runTimers()
  await settle()

  const view = hub.view()
  expect(view.showGlyphs).toBe(true)
  expect(view.machines.map(m => [m.machineId, m.showGlyph, m.isLocal])).toEqual([['mac-1', false, true], ['vps-1', true, false]])
  // The same repository on both machines is one project.
  expect(view.projects.map(p => [p.name, p.instances.map(i => i.machineId)])).toEqual([['kybern', ['mac-1', 'vps-1']], ['site', ['vps-1']]])
  expect(view.threads.all.map(t => [t.machineId, t.thread.id])).toEqual([['mac-1', 't-same'], ['vps-1', 't-v'], ['mac-1', 't-m'], ['vps-1', 't-same']])

  const before = vps.calls.length
  await hub.clientForThread('t-v').call('threads.list', {})
  expect(vps.calls.slice(before)).toEqual(['threads.list'])
  expect(hub.machineOfThread('t-v')).toBe('vps-1')
  // The same id on two machines: the caller's machine decides, else the one the user is at.
  expect(hub.machineOfThread('t-same')).toBe('mac-1')
  expect(hub.machineOfThread('t-same', 'vps-1')).toBe('vps-1')
  expect(hub.clientForThread('t-same', 'vps-1')).toBe(hub.clientFor('vps-1'))
  expect(hub.clientForProject('p10')).toBe(hub.clientFor('vps-1'))
  expect(hub.clientForProject('p1')).toBe(hub.clientFor('mac-1'))
  expect(mac.calls).toEqual([])
})

test('an offline machine stays listed from its last read, greyed, and its client says so', async () => {
  const vps = new FakeDaemon('vps-1', { 'projects.list': () => ({ projects: [project('p9', 'site', 'r2')] }), 'threads.list': () => ({ threads: [thread('t-v', 'p9', '2026-10-03T00:00:00Z')] }) })
  const { hub, runTimers } = makeHub([saved('vps', { environment_id: 'vps-1' })], { transport: { createClient: () => vps as never } })
  await openPrimary(hub, new FakeDaemon('mac-1'), undefined, { projects: [], threads: [] })
  await hub.set.ensureAll()
  runTimers()
  vps.online = false
  vps.setStatus('closed')
  await settle()
  const view = hub.view()
  expect(view.machines.find(m => m.machineId === 'vps-1')?.offline).toBe(true)
  expect(view.threads.all.map(t => [t.machineId, t.offline])).toEqual([['vps-1', true]])
  const error = (() => { try { hub.clientFor('vps-1') } catch (e) { return e as OfflineError } })()
  expect(error).toBeInstanceOf(OfflineError)
  expect(error?.cached).toBe(true)
})

test('lists changed on another machine are read again once, however many notifications arrive during a read', async () => {
  let reads = 0
  let hold = false
  let release: () => void = () => {}
  const vps = new FakeDaemon('vps-1', {
    'projects.list': async () => { reads++; if (hold) await new Promise<void>(resolve => { release = resolve }); return { projects: [] } },
    'threads.list': () => ({ threads: [] }),
  })
  const { hub } = makeHub([saved('vps', { environment_id: 'vps-1' })], { transport: { createClient: () => vps as never } })
  await openPrimary(hub, new FakeDaemon('mac-1'), undefined, { projects: [], threads: [] })
  await hub.set.ensureAll()
  await settle()
  const initial = reads
  hold = true
  vps.notify('threads.changed')
  await settle()
  hold = false
  vps.notify('threads.changed'); vps.notify('projects.changed'); vps.notify('threads.changed')
  await settle()
  release()
  await settle()
  expect(reads - initial).toBe(2)
})

test('successful project setup waits for an overlapping notification refresh before selecting the machine', async () => {
  const source = project('source', 'kybern-website', 'website-root')
  const destination = project('destination', 'kybern-website', 'website-root')
  let projects: Project[] = []
  let hold = false
  let release: () => void = () => {}
  const remote = new FakeDaemon('pc', {
    'projects.list': async () => {
      const snapshot = projects
      if (hold) await new Promise<void>(resolve => { release = resolve })
      return { projects: snapshot }
    },
    'threads.list': () => ({ threads: [] }),
  })
  const { hub } = makeHub([saved('pc', { environment_id: 'pc' })], { transport: { createClient: () => remote.asClient() } })
  await openPrimary(hub, new FakeDaemon('mac-1'), undefined, { projects: [source], threads: [] })
  await hub.set.ensureAll()
  await settle()
  const key = hub.view().projects.find(item => item.instances.some(instance => instance.project.id === source.id))!.key
  hold = true
  remote.notify('threads.changed')
  await settle()
  // Setup finishes while an older list response is still on its way back.
  projects = [destination]
  hold = false
  const selected: string[] = []
  let outcome = 'pending'
  const completion = finishMachineSetup({ projectId: source.id, key, targetId: 'pc', paneId: 'original-pane' }, {
    refresh: id => hub.refreshMachineLists(id), projects: () => hub.view().projects, localMachineId: hub.localMachineId,
    paths: async () => {}, providers: async () => {}, select: (_key, machineId) => { selected.push(machineId) },
  }).then(() => { outcome = 'selected' }, error => { outcome = error.message })
  await settle()
  const beforeResponse = outcome
  release()
  await completion
  await settle()
  expect(beforeResponse).toBe('pending')
  expect(outcome).toBe('selected')
  expect(selected).toEqual(['pc'])
})

test('the primary is never dialed by the set, and a stand-in profile record does not survive its daemon’s real id', async () => {
  const created: string[] = []
  const { hub } = makeHub([saved('vps', { environment_id: 'vps-1' })], { transport: { createClient: ({ record }) => { created.push(record.machineId); return new FakeDaemon(record.machineId) as never } } })
  await openPrimary(hub, new FakeDaemon('mac-1'))
  hub.releasePrimary()
  await settle()
  await hub.set.networkChanged()
  await settle()
  expect(created.filter(id => id === 'mac-1')).toEqual([])
  expect(hub.connections().filter(c => c.record.legacyProfileId === 'local').map(c => c.record.machineId)).toEqual(['mac-1'])
})

test('a saved environment’s stand-in record keeps its routes and place when its daemon answers as the primary', async () => {
  const { hub } = makeHub([saved('box', { ssh: { target: 'me@box', remote_port: 4173, local_port: 5555 } })])
  await openPrimary(hub, new FakeDaemon('box-1'), { id: 'box', name: 'box' })
  expect(hub.connections().map(c => [c.record.machineId, c.record.legacyProfileId, c.status])).toEqual([['box-1', 'box', 'needs-update'], [`${LEGACY_PREFIX}local`, 'local', 'offline']])
  expect(hub.set.connection('box-1')?.record.routes).toEqual([{ kind: 'ssh', url: 'http://127.0.0.1:5555' }])
  expect(hub.localMachineId).toBe('box-1')
})

test('a saved SSH profile survives discovery creating the canonical row before it answers as primary', async () => {
  const ssh = { target: 'me@box', remote_port: 4173, local_port: 5555 }
  const records = recordsFromRegistry({ selected_id: 'box', environments: [saved('box', { ssh })] }, local)
  Object.assign(records.find((record) => record.legacyProfileId === 'box')!, { ssh })
  records.push({ machineId: 'box-1', name: 'Box', member: true, signingKey: 'network-pin', routes: [] })
  const { hub } = makeHub([], { records: () => records })
  await openPrimary(hub, new FakeDaemon('box-1'), { id: 'box', name: 'box' })
  const box = hub.set.connection('box-1')!
  expect(hub.connections().filter((c) => c.record.machineId.includes('box'))).toHaveLength(1)
  expect(box.record.legacyProfileId).toBe('box')
  expect(box.record.routes).toEqual([{ kind: 'ssh', url: 'http://127.0.0.1:5555' }])
  expect(box.record.signingKey).toBe('network-pin')
  expect((box.record as typeof box.record & { ssh: typeof ssh }).ssh).toEqual(ssh)
  expect(hub.localMachineId).toBe('box-1')
})

test('the activity the app read with the primary’s threads reaches the set with the lists', async () => {
  const { hub } = makeHub()
  const daemon = new FakeDaemon('mac-1')
  hub.beginPrimary(daemon.asClient(), { id: 'local', name: 'This Mac' })
  daemon.setStatus('open')
  const adopted = hub.primaryOpened(daemon.asClient())
  const activity = [{ thread_id: 't1', line: 'Using Bash' }] as never
  hub.ingestPrimary(daemon.asClient(), [], [thread('t1', 'p1', '2026-10-02T00:00:00Z')], activity)
  await adopted
  await settle()
  expect(hub.set.activityOf('mac-1')).toEqual(activity)
  expect(daemon.calls).toEqual([])
})

test('another machine’s events reach a listener with its machine id; one machine opens no stream', async () => {
  const vps = new FakeDaemon('vps-1', { 'projects.list': () => ({ projects: [] }), 'threads.list': () => ({ threads: [] }) })
  const mac = new FakeDaemon('mac-1')
  const { hub } = makeHub([saved('vps', { environment_id: 'vps-1' })], { transport: { createClient: () => vps as never } })
  const events: [string, string][] = []
  const stop = hub.onMachineEvent((machineId, event) => events.push([machineId, event.kind]))
  await openPrimary(hub, mac, undefined, { projects: [], threads: [] })
  // The machine the user is at is the app's own socket: the hub never subscribes on it.
  expect(mac.streams).toEqual([])
  await hub.set.ensureAll()
  await settle()
  expect(vps.streams.filter(s => s.open).length).toBe(1)
  vps.emit({ kind: 'thread_updated', seq: 1 })
  mac.emit({ kind: 'thread_updated', seq: 1 })
  expect(events).toEqual([['vps-1', 'thread_updated']])
  stop()
  expect(vps.streams.filter(s => s.open).length).toBe(0)
})

// Every machine's stream starts once at launch: that start reads nothing again (its lists were just read). A later start
// that cannot replay what it missed does.
test('a machine stream re-reads its lists when it restarts without replay, not when it first opens', async () => {
  const vps = new FakeDaemon('vps-1', { 'projects.list': () => ({ projects: [] }), 'threads.list': () => ({ threads: [] }) })
  const mac = new FakeDaemon('mac-1')
  const { hub } = makeHub([saved('vps', { environment_id: 'vps-1' })], { transport: { createClient: () => vps as never } })
  hub.onMachineEvent(() => {})
  await openPrimary(hub, mac, undefined, { projects: [], threads: [] })
  await hub.set.ensureAll()
  await settle()
  const [stream] = vps.streams.filter(s => s.open)
  const reads = () => vps.calls.filter(method => method === 'threads.list').length
  const before = reads()
  stream!.onSubscribed?.(5, { resumed: false, supported: true })
  await settle()
  expect(reads()).toBe(before)
  stream!.onSubscribed?.(9, { resumed: true, supported: true })
  await settle()
  expect(reads()).toBe(before)
  stream!.onSubscribed?.(12, { resumed: false, supported: true })
  await settle()
  expect(reads()).toBe(before + 1)
})

test('without another machine the event listener opens nothing', async () => {
  const mac = new FakeDaemon('mac-1')
  const { hub } = makeHub()
  hub.onMachineEvent(() => {})
  await openPrimary(hub, mac, undefined, { projects: [], threads: [] })
  await hub.connectOthers()
  await settle()
  expect(mac.streams).toEqual([])
  expect(hub.machineCount).toBe(1)
})

test('a machine that is removed closes its event stream', async () => {
  const vps = new FakeDaemon('vps-1', { 'projects.list': () => ({ projects: [] }), 'threads.list': () => ({ threads: [] }) })
  const { hub } = makeHub([saved('vps', { environment_id: 'vps-1' })], { transport: { createClient: () => vps as never } })
  const events: string[] = []
  hub.onMachineEvent((_, event) => events.push(event.kind))
  await openPrimary(hub, new FakeDaemon('mac-1'), undefined, { projects: [], threads: [] })
  await hub.set.ensureAll()
  await settle()
  vps.emit({ kind: 'thread_updated', seq: 1 })
  await hub.set.removeMachine('vps-1')
  await settle()
  // The removed machine's connection may still be draining: it is not heard.
  vps.emit({ kind: 'thread_updated', seq: 2 })
  expect(events).toEqual(['thread_updated'])
})

test("the app's own connection to a relayed machine sends the member's token on HTTP too (ADE-108)", async () => {
  const seen: { path: string; auth: string | null; relay: string | null }[] = []
  const server = Bun.serve({ hostname: '127.0.0.1', port: 0, fetch(request) { seen.push({ path: new URL(request.url).pathname, auth: request.headers.get('authorization'), relay: request.headers.get('x-kybern-relay-auth') }); return new Response('ok') } })
  try {
    const hub = new MachineHub({ records: () => [] })
    const create = (hub as unknown as { createClient: (args: Parameters<MachineTransport['createClient']>[0]) => KybernClient }).createClient.bind(hub)
    const record = { machineId: 'instabox', name: 'instabox', routes: [], member: true }
    const member = `http://127.0.0.1:${server.port}`
    await create({ record, route: { kind: 'relay', url: `${member}/relay/instabox`, via: 'mac' }, token: 'tok-instabox', relayToken: 'tok-mac' }).fetch('/assets/a1')
    await create({ record, route: { kind: 'direct_tailnet', url: member }, token: 'tok-instabox' }).fetch('/assets/a1')
    expect(seen).toEqual([
      { path: '/relay/instabox/http/assets/a1', auth: 'Bearer tok-instabox', relay: 'Bearer tok-mac' },
      { path: '/assets/a1', auth: 'Bearer tok-instabox', relay: null },
    ])
  } finally { server.stop(true) }
})
