// GPUIX's machine set (design §5.2, steps 1 and 2).
//
// The hub owns a `MachineSet` and the seams between it and the app as it is:
//
// - The daemon connection `state.ts` opens (the active environment) is the
//   "primary" machine, the one the user is at. It joins the set once the daemon
//   has answered and its machine id is known, as a connection the set adopts but
//   does not own, so its socket, status details and reconnects stay exactly as
//   they were and the set never reads lists the app already has.
// - Saved machines are loaded as records (`store.ts`). With one machine the app
//   opens the same single socket it always did and nothing else is dialed.
// - Other machines (connected by the set itself) are read by the set and their
//   lists merged with the primary's: `state.ts` shows the merged view.
//
// The set connects the other machines itself (`connectOthers`: saved machines
// and the rest of the network, learned from the primary when the daemon is a
// member). The app keeps opening its own socket to the machine the user is at.
import { KybernClient, type ClientOptions, type Subscription } from '../../../../packages/kybern-client/src/client.ts'
import { httpBase, normalizeDaemonUrl } from '../../../../packages/kybern-client/src/address.ts'
import { MachineCache } from '../../../../packages/kybern-client/src/machineCache.ts'
import {
  MachineSet,
  OfflineError,
  UnknownMachineError,
  probeAuth,
  routeHttp,
  type Clock,
  type CredentialStore,
  type MachineConnection,
  type MachineRecord,
  type MachineSetEvent,
  type MachineStore,
  type MachineTransport,
  type MergedView,
  type SocketFactory,
} from '../../../../packages/kybern-client/src/machines.ts'
import { MachineClientProxy } from '../../../../packages/kybern-client/src/machineClient.ts'
import { NETWORK_FEATURES, type MachineId, type NetworkStatus, type Project, type Thread, type ThreadActivitySummary, type ThreadEvent } from '../../../../packages/kybern-client/src/types.ts'
import { verifyMachineProof } from './proof.ts'
import { createGpuixSocket } from './socket.ts'
import { openRelay } from '../preview/relay.ts'
import { isLegacyMachineId } from './records.ts'

export interface PrimaryProfile { id: string; name: string }

/**
 * The client of a machine the set dials, on the route it chose. A relay route sends the member's own token beside the
 * target's, on the socket and on HTTP. `probeAuth` tells a refused token from a machine that is down: a native socket
 * refused at the upgrade shows no status, and without the check a revoked token looked like an unreachable machine,
 * so the set never minted a fresh one (ADE-124).
 */
export function machineClientOptions(args: Parameters<MachineTransport['createClient']>[0], socketFactory: SocketFactory = createGpuixSocket): { url: string; options: ClientOptions } {
  const { record, route, token, relayToken } = args
  const legacy = isLegacyMachineId(record.machineId)
  const http = routeHttp(route, relayToken)
  const socket: SocketFactory = http.headers ? (url, bearer) => socketFactory(url, bearer, http.headers) : socketFactory
  const url = normalizeDaemonUrl(route.url)
  return {
    url,
    options: {
      createSocket: socket,
      probeAuth: () => probeAuth(url, token, http.headers, route.kind === 'relay'),
      http,
      // Its pages (artifact previews, visual frames, Preview) load from a loopback relay that adds the member's token itself.
      pageRelay: (upstream, options) => openRelay(upstream, httpBase(route.url), { ...options, relayAuth: relayToken }),
      ...(legacy ? {} : { expectedEnvironmentId: record.machineId }),
    },
  }
}

export interface MachineHubOptions {
  /** The saved machines, as records. Read once, when the set loads. */
  records: () => MachineRecord[] | Promise<MachineRecord[]>
  /** Where the set saves its records (default: nowhere). */
  save?: (records: MachineRecord[]) => void | Promise<void>
  /** A saved environment's device token, by profile id (`records.ts` stand-ins). */
  readCredential?: (profileId: string) => Promise<string>
  /** The set's credential store (default: reads `readCredential` by the record's profile id, writes nothing). */
  credentials?: CredentialStore
  /** The offline cache (default: in memory). */
  cache?: MachineCache
  /** Before any machine is dialed: a tunnel for a saved SSH machine, say. Failures never stop the others. */
  prepare?: (record: MachineRecord) => void | Promise<void>
  /** A saved profile's token was replaced by `machine:<id>`: drop the old one. */
  forgetProfile?: (profileId: string) => void | Promise<void>
  /** The network status of the daemon the app is at, once read (the one-time notice). */
  onNetwork?: (status: NetworkStatus) => void
  /** This device's id for `network.client_grant`. */
  deviceId?: string
  /** Replaces the sockets of non-primary machines (tests). */
  transport?: Partial<MachineTransport>
  clock?: Clock
  /** Delay before the primary's lists reach the set (default 300 ms); one write per burst of `threads.changed`. */
  ingestDelayMs?: number
  setTimer?: (fn: () => void, ms: number) => unknown
  clearTimer?: (handle: unknown) => void
}

interface Primary {
  client: KybernClient
  profile: PrimaryProfile
  machineId?: MachineId
  /** The set has this client as the machine's connection. */
  adopted?: boolean
  /** The set got the first lists of this connection (later ones wait until something reads them). */
  listed?: boolean
}

interface PrimaryLists { owner: KybernClient; projects: Project[]; threads: Thread[]; activity?: ThreadActivitySummary[]; ingested: boolean }

const LIVE = new Set(['online', 'relaying', 'needs-update'])

export class MachineHub {
  readonly set: MachineSet
  private readonly options: MachineHubOptions
  private primary: Primary | null = null
  private lists: PrimaryLists | null = null
  /** Every machine id that has been the primary in this run: the app's own socket carries it, the set never dials it. */
  private readonly adopted = new Set<MachineId>()
  private readonly watched = new Set<MachineId>()
  /** Live event streams of the machines the set connects itself, by the machine's proxy (it survives routes and re-keys). */
  private readonly streams = new Map<KybernClient, Subscription>()
  private readonly eventListeners = new Set<(machineId: MachineId, event: ThreadEvent) => void>()
  private readonly refreshes = new Map<MachineId, { again: boolean; done: Promise<void> }>()
  private readonly listeners = new Set<() => void>()
  /** Told when the machines or their status change (not when lists do); watching is free. */
  private readonly machineListeners = new Set<() => void>()
  private flushTimer: unknown
  private disposed = false

  constructor(options: MachineHubOptions) {
    this.options = options
    const store: MachineStore = {
      load: async () => {
        const records = await options.records()
        // A machine the app dials needs its tunnel first (a saved SSH machine).
        if (options.prepare) await Promise.all(records.map((record) => Promise.resolve(options.prepare!(record)).catch(() => {})))
        return records
      },
      save: (records) => options.save?.(records),
    }
    this.set = new MachineSet({
      store,
      credentials: options.credentials ?? this.credentials(),
      deviceId: options.deviceId ?? 'gpuix-desktop',
      verifyProof: verifyMachineProof,
      cache: options.cache ?? new MachineCache(),
      socket: createGpuixSocket,
      transport: { ...options.transport, createClient: (args) => this.createClient(args) },
      ...(options.clock ? { clock: options.clock } : {}),
    })
    this.set.onChange((event) => this.onSetChange(event))
  }

  // ---- the primary: the connection state.ts owns ----

  /** `state.ts` built a client for the active environment. */
  beginPrimary(client: KybernClient, profile: PrimaryProfile) {
    this.primary = { client, profile }
    this.lists = null
  }

  /** The primary client opened: join the set under the daemon's machine id. */
  async primaryOpened(client: KybernClient): Promise<void> {
    const primary = this.primary
    if (!primary || primary.client !== client || this.disposed) return
    const info = client.info
    const machineId = info?.machine_id || info?.environment_id
    if (!machineId) return
    // A reconnect of the same daemon: the set already follows the client's own status.
    if (primary.machineId === machineId && this.set.connection(machineId)?.status !== 'paused') return
    primary.machineId = machineId
    this.adopted.add(machineId)
    await this.set.load()
    // The saved record of this profile had a stand-in id until the daemon answered: it becomes the daemon's id.
    for (const other of this.set.connections()) {
      const record = other.record
      if (record.legacyProfileId !== primary.profile.id || record.machineId === machineId || !isLegacyMachineId(record.machineId)) continue
      if (this.set.connection(machineId)) await this.set.mergeSavedMachine(record.machineId, machineId)
      else await this.set.rekeyMachine(record.machineId, machineId)
      await this.forgetProfile(record.legacyProfileId)
    }
    if (this.primary?.client !== client || this.disposed) return
    // A daemon with the network on is a member: the set learns the other machines from it.
    const member = await this.isMember(client)
    if (this.primary?.client !== client || this.disposed) return
    // The set never dials this machine and never reads its lists: the app's socket is the connection and `ingestPrimary` hands over the lists.
    await this.set.adoptConnection({
      machineId,
      name: primary.profile.name,
      member,
      paused: false,
      legacyProfileId: primary.profile.id,
    }, client, { autoRefresh: false })
    if (this.primary?.client !== client) return
    primary.adopted = true
    // The set starts with the lists the app already read, as it did when it read them through the app's client.
    this.ingestNow(primary)
    this.set.setLocalMachine(machineId)
    this.watchOthers()
    this.scheduleFlush()
    // The rest of the network and the saved machines, in the background: the app already works.
    void this.connectOthers().catch(() => {})
  }

  /** Connect every machine but the primary and read their lists (idempotent while the set is loaded). */
  async connectOthers(): Promise<void> {
    if (this.disposed) return
    await this.set.ensureAll()
    this.watchOthers()
    await this.rekeyLegacy()
  }

  /** Does the daemon at `client` belong to a machine network (so it can name the other machines)? */
  private async isMember(client: KybernClient): Promise<boolean> {
    if (client.info?.features && !client.info.features.includes(NETWORK_FEATURES.network)) return false
    try {
      const status = await client.call('network.status', {})
      try { this.options.onNetwork?.(status) } catch { /* a notice never touches the connection */ }
      const id = client.info?.machine_id || client.info?.environment_id
      return !!status.enabled && !status.removed && status.self_machine.machine_id === id && status.members.some((member) => member.machine_id === id)
    } catch {
      return false
    }
  }

  private async forgetProfile(profileId: string | undefined) {
    // The local daemon has no saved token: its credentials are the data folder's files.
    if (!profileId || profileId === 'local' || !this.options.forgetProfile) return
    try { await this.options.forgetProfile(profileId) } catch { /* the token is unused once the record moved */ }
  }

  /** A saved machine that answered: its stand-in id becomes the daemon's. */
  private async rekeyLegacy(): Promise<void> {
    for (const connection of this.set.connections()) {
      const record = connection.record
      const id = connection.info?.machine_id || connection.info?.environment_id
      if (!isLegacyMachineId(record.machineId) || !id || id === record.machineId) continue
      try {
        if (this.set.connection(id)) await this.set.mergeSavedMachine(record.machineId, id)
        else await this.set.rekeyMachine(record.machineId, id)
        await this.forgetProfile(record.legacyProfileId)
      } catch { /* retried when it answers again */ }
    }
  }

  /** The primary client was closed (an environment switch, a reconnect from scratch). */
  releasePrimary() {
    const primary = this.primary
    if (!primary) return
    this.primary = null
    this.lists = null
    this.cancelFlush()
    // Paused, so the set does not retry a socket that only the app can open.
    if (primary.machineId) void this.set.setPaused(primary.machineId, true)
    this.syncStreams()
  }

  /** The primary's lists reach the set now, not after the delay (`state.ts` is about to read the merged view). */
  ingestPrimaryNow() {
    if (this.primary) this.ingestNow(this.primary)
  }

  /** The lists `state.ts` just read from the primary daemon (all threads, children and archived included). */
  ingestPrimary(client: KybernClient, projects: Project[], threads: Thread[], activity?: ThreadActivitySummary[]) {
    if (this.primary?.client !== client) return
    this.lists = { owner: client, projects, threads, ...(activity ? { activity } : {}), ingested: false }
    if (this.primary.adopted && !this.primary.listed) this.ingestNow(this.primary)
    this.scheduleFlush()
  }

  // ---- reading ----

  /** The machine the user is at: where `getClient()` points. */
  get localMachineId(): MachineId | undefined { return this.set.localMachineId }
  get primaryClient(): KybernClient | null { return this.primary?.client ?? null }

  /** More than one machine is known: today only a saved environment that is not the active one. */
  get machineCount(): number { return this.set.connections().length }

  /** The merged lists of every machine. Stable between changes. */
  view(): MergedView {
    return this.set.merged()
  }

  subscribe(listener: () => void): () => void {
    this.listeners.add(listener)
    this.scheduleFlush()
    return () => { this.listeners.delete(listener) }
  }

  connections(): readonly MachineConnection[] { return this.set.connections() }

  /** Machines and statuses only. Unlike `subscribe` it never makes the hub keep the merged lists. */
  onMachinesChanged(listener: () => void): () => void {
    this.machineListeners.add(listener)
    return () => { this.machineListeners.delete(listener) }
  }

  /**
   * Every thread event of every machine but the one the user is at (`thread_updated`, `turn_completed`, requests…),
   * with the machine that raised it. The streams open while someone listens and another machine is connected, follow
   * the machine across reconnects and re-keys (replaying what they missed), and close when it is removed. One machine
   * opens none.
   */
  onMachineEvent(listener: (machineId: MachineId, event: ThreadEvent) => void): () => void {
    this.eventListeners.add(listener)
    this.syncStreams()
    return () => {
      this.eventListeners.delete(listener)
      this.syncStreams()
    }
  }

  /** Read one machine's lists again, waiting for any queued reread as well as the current read. */
  refreshMachineLists(machineId: MachineId): Promise<void> {
    return this.refreshMachine(machineId).catch(() => {})
  }

  /** A machine's client (throws `OfflineError` while it cannot be reached). */
  clientFor(machineId: MachineId): KybernClient {
    return this.set.clientFor(machineId)
  }

  /**
   * A machine's client, connecting to it first when it is not up: for something the user just opened there. Dials at
   * once (out of any backoff) and waits up to `timeoutMs`; rejects with `OfflineError` when it still cannot be reached.
   */
  async reachMachine(machineId: MachineId, timeoutMs?: number): Promise<KybernClient> {
    try { return this.set.clientFor(machineId) } catch (error) { if (!(error instanceof OfflineError)) throw error }
    await this.set.reach(machineId, timeoutMs)
    return this.set.clientFor(machineId)
  }

  /**
   * The client of the machine that owns a thread. Thread ids are unique per
   * machine only, so pass `machineId` when the caller has it; an unlisted thread
   * (a new chat) belongs to the machine the user is at.
   */
  clientForThread(threadId: string, machineId?: MachineId): KybernClient {
    return this.set.clientFor(machineId ?? this.set.machineOfThread(threadId) ?? this.requireLocal())
  }

  /** The client of the machine that owns a project instance; `machineId` picks one of several checkouts. */
  clientForProject(projectId: string, machineId?: MachineId): KybernClient {
    return this.set.clientFor(machineId ?? this.set.machineOfProject(projectId) ?? this.requireLocal())
  }

  machineOfThread(threadId: string, hint?: MachineId): MachineId | undefined {
    return this.set.machineOfThread(threadId, hint) ?? this.set.localMachineId
  }

  dispose() {
    this.disposed = true
    this.cancelFlush()
    for (const stream of this.streams.values()) stream.unsubscribe()
    this.streams.clear()
    this.eventListeners.clear()
    this.set.dispose()
    this.listeners.clear()
    this.machineListeners.clear()
  }

  // ---- internals ----

  private requireLocal(): MachineId {
    const id = this.set.localMachineId
    if (!id) throw new UnknownMachineError('local')
    return id
  }

  private credentials(): CredentialStore {
    const profileOf = (machineId: MachineId) => this.set.connection(machineId)?.record.legacyProfileId
    return {
      get: async (machineId) => {
        const profile = profileOf(machineId)
        if (!profile || !this.options.readCredential) return undefined
        try { return await this.options.readCredential(profile) } catch { return undefined }
      },
      // The registry and credential store stay as they are until the migration (step 5).
      set: () => {},
      delete: () => {},
    }
  }

  private createClient(args: Parameters<MachineTransport['createClient']>[0]) {
    if (this.options.transport?.createClient) return this.options.transport.createClient(args)
    const { record, token } = args
    const { url, options } = machineClientOptions(args)
    return new KybernClient({ url, token, ...(isLegacyMachineId(record.machineId) ? {} : { environmentId: record.machineId }) }, options)
  }

  private onSetChange(event: MachineSetEvent) {
    if (event.type === 'records' || event.type === 'status') {
      this.watchOthers()
      this.scheduleFlush()
      if (event.type === 'status') void this.rekeyLegacy().catch(() => {})
      for (const listener of [...this.machineListeners]) listener()
    }
    for (const listener of [...this.listeners]) listener()
  }

  /** Lists changed on a machine the set connects itself: read that machine again (state.ts does it for the primary). */
  private watchOthers() {
    this.syncStreams()
    for (const connection of this.set.connections()) {
      const id = connection.record.machineId
      if (this.watched.has(id) || this.adopted.has(id) || (connection.record.legacyProfileId && isLegacyMachineId(id))) continue
      this.watched.add(id)
      const handle = this.set.handleFor(id)
      const refresh = () => void this.refreshMachine(id).catch(() => {})
      handle.onNotification('projects.changed', refresh)
      handle.onNotification('threads.changed', refresh)
    }
  }

  /** Open an event stream for each machine the set connects itself while someone listens; close the streams of machines that left. */
  private syncStreams() {
    const wanted = new Map<KybernClient, MachineId>()
    if (!this.disposed && this.eventListeners.size > 0 && this.primary?.adopted) {
      for (const connection of this.set.connections()) {
        const id = connection.record.machineId
        if (this.adopted.has(id) || (connection.record.legacyProfileId && isLegacyMachineId(id))) continue
        wanted.set(connection.client, id)
      }
    }
    for (const [client, stream] of this.streams) {
      if (wanted.has(client)) continue
      stream.unsubscribe()
      this.streams.delete(client)
    }
    for (const [client] of wanted) {
      if (this.streams.has(client)) continue
      // The proxy keeps this subscription while the machine is offline and re-issues it from its cursor on the next route.
      const stream: { sub?: Subscription; opened?: boolean } = {}
      // The machine's id now, not when the stream opened: a stand-in id is re-keyed once the daemon answers.
      const machineOf = () => (client instanceof MachineClientProxy ? client.machineId : wanted.get(client))
      stream.sub = client.subscribeEvents({ include_tool_output: false }, (event) => {
        // A stream that was closed (the machine was removed) says nothing more.
        if (this.streams.get(client) !== stream.sub || this.disposed) return
        const machineId = machineOf()
        if (!machineId) return
        for (const listener of [...this.eventListeners]) listener(machineId, event)
      }, (_head, replay) => {
        // A stream that starts again without replaying what it missed (opened again after the app's own daemon
        // reconnected, or a cursor the machine no longer knows, as after a restore) cannot patch the lists up: they are
        // read again (ADE-121). Its first start has nothing to catch up: the lists were read as the machine connected.
        const first = !stream.opened
        stream.opened = true
        if (first || (replay.resumed && replay.supported) || this.streams.get(client) !== stream.sub || this.disposed) return
        const machineId = machineOf()
        if (machineId) void this.refreshMachine(machineId).catch(() => {})
      })
      this.streams.set(client, stream.sub)
    }
  }

  /** One read per machine at a time; a change that arrives during a read folds into one rerun. */
  private refreshMachine(id: MachineId): Promise<void> {
    const pending = this.refreshes.get(id)
    if (pending) { pending.again = true; return pending.done }
    const refresh = { again: false, done: Promise.resolve() }
    // Publish the shared promise before starting a read: notifications and setup
    // completion may arrive together, and every caller must await the fresh lists.
    refresh.done = Promise.resolve().then(async () => {
      try {
        do {
          refresh.again = false
          await this.set.refresh(id)
        } while (refresh.again && !this.disposed)
      } finally {
        this.refreshes.delete(id)
      }
    })
    this.refreshes.set(id, refresh)
    return refresh.done
  }

  // The set caches and merges whatever it is given. With one machine and nothing
  // watching the merged view that work has no reader, so the primary's lists wait
  // (by reference) until a second machine is live or a view subscribes.
  private wanted(): boolean {
    if (this.listeners.size > 0) return true
    let live = 0
    for (const connection of this.set.connections()) if (LIVE.has(connection.status)) live++
    return live > 1
  }

  private cancelFlush() {
    if (this.flushTimer === undefined) return
    if (this.options.clearTimer) this.options.clearTimer(this.flushTimer)
    else clearTimeout(this.flushTimer as ReturnType<typeof setTimeout>)
    this.flushTimer = undefined
  }

  private scheduleFlush() {
    if (this.flushTimer !== undefined || this.disposed || !this.lists || this.lists.ingested || !this.primary?.machineId || !this.wanted()) return
    const setTimer = this.options.setTimer ?? ((fn, ms) => setTimeout(fn, ms))
    this.flushTimer = setTimer(() => {
      this.flushTimer = undefined
      if (this.primary) this.ingestNow(this.primary)
    }, this.options.ingestDelayMs ?? 300)
  }

  private ingestNow(primary: Primary) {
    const lists = this.lists
    if (!lists || lists.ingested || !primary.machineId || primary.client !== lists.owner) return
    lists.ingested = true
    primary.listed = true
    this.set.ingest(primary.machineId, { projects: lists.projects, threads: lists.threads, ...(lists.activity ? { activity: lists.activity } : {}) })
  }
}
