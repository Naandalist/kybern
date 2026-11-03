// The phone's machine set (design §5.3).
//
// - The connection `runtime.ts` opens (the computer the phone is at) is the
//   "primary". The set adopts it as that machine's connection (`adoptConnection`),
//   so its socket, status, reconnects and saved-credentials-before-socket rule
//   stay exactly as they were, and the set never dials, retries or closes it.
// - Every other machine is the set's own: it finds a route, verifies the machine's
//   identity, connects with the token this device holds (or one `network.client_grant`
//   mints through a member) and keeps the machine's lists, offline cache included.
// - The primary's lists reach the set from `runtime.ts` (it already reads them), once
//   per burst of changes, and only while a second machine or a subscriber could use them.
import { KybernClient, type ClientOptions, type DaemonPage, type PageRelay } from "../../../../packages/kybern-client/src/client";
import { httpBase, normalizeDaemonUrl } from "../../../../packages/kybern-client/src/address";
import { MachineCache } from "../../../../packages/kybern-client/src/machineCache";
import {
  MachineSet,
  probeAuth,
  randomNonce,
  type RandomFill,
  relayHeadersFor,
  routeHttp,
  UnknownMachineError,
  type Clock,
  type CredentialStore,
  type MachineConnection,
  type MachineRecord,
  type MachineSetEvent,
  type MachineStore,
  type MachineTransport,
  type MergedView,
  type SocketFactory,
} from "../../../../packages/kybern-client/src/machines";
import { NETWORK_FEATURES, type MachineId, type Route, type Project, type Thread, type ThreadActivitySummary } from "../../../../packages/kybern-client/src/types";
import { verifyMachineProof } from "./proof";
import { isLegacyMachineId, routeUrl } from "./records";

export interface PrimaryProfile {
  /** The machine id the phone saved the computer under (a stand-in until the daemon answers). */
  machineId: MachineId;
  name: string;
  /** The saved address, for a computer the set has no record of yet. */
  url?: string;
}

export interface PrimaryLists {
  projects: Project[];
  threads: Thread[];
  activity: ThreadActivitySummary[];
}

export interface MachineHubOptions {
  store: MachineStore;
  credentials: CredentialStore;
  /** The offline cache; omit to keep none. */
  cache?: MachineCache;
  /** This device's id for `network.client_grant`. */
  deviceId: string;
  /**
   * The phone's secure random source (expo-crypto). Hermes has no `globalThis.crypto`, so without it the identity
   * check's nonce cannot be made and no machine but the paired one ever connects.
   */
  randomFill?: RandomFill;
  /** The socket other machines open (the native one on a phone). */
  socket?: SocketFactory;
  /** Replaces the sockets of non-primary machines (tests). */
  transport?: Partial<MachineTransport>;
  clock?: Clock;
  /** The primary's machine id changed (a stand-in became the daemon's real id). */
  onRekey?: (oldId: MachineId, newId: MachineId) => void;
  /** Delay before the primary's lists reach the set (default 300 ms); one write per burst of changes. */
  ingestDelayMs?: number;
  setTimer?: (fn: () => void, ms: number) => unknown;
  clearTimer?: (handle: unknown) => void;
}

interface Primary {
  client: KybernClient;
  profile: PrimaryProfile;
  machineId: MachineId;
  lists?: () => PrimaryLists;
}

const LIVE = new Set(["online", "relaying", "needs-update"]);

/** A page of another machine: what the WebView loads, the prefix that stays in it, and how to give it back. */
export type MachinePage = DaemonPage & {
  /** Set for a relayed page: the ticket root on the relay member. A direct page's scope is its own ticket root. */
  scope?: string;
};

/** The relay member a page of a relayed machine goes through. */
export interface RelayPageHost {
  name: string;
  /** The member announces `relay.page.v1`. */
  hasRelayPages: boolean;
  /** The member's client; throws `OfflineError` while it is offline. */
  client(): KybernClient;
}

export interface MachineClientDeps {
  /** The native socket (it takes the relay headers as a third argument). */
  socket?: SocketFactory;
  /** The relay member of a relay route, by machine id. */
  relayPages?(viaId: MachineId): RelayPageHost | undefined;
}

/**
 * The client of a machine this phone is not at, for the route the set chose. A relay route sends the
 * relay member's token beside the machine's own on the socket and on HTTP, and opens pages through a
 * ticket on the member, because a WebView cannot send that header.
 */
export function createMachineClient(
  args: { record: MachineRecord; route: Route; token: string; relayToken?: string },
  deps: MachineClientDeps = {},
): KybernClient {
  const { record, route, token, relayToken } = args;
  const legacy = isLegacyMachineId(record.machineId);
  const relay = route.kind === "relay";
  const relayHeaders = relay ? relayHeadersFor(relayToken) : undefined;
  if (relay && (!relayHeaders || !deps.socket)) throw new Error("A relay route needs the relay member's token and a native socket");
  const socket: SocketFactory | undefined =
    deps.socket && relayHeaders ? (url, bearer) => deps.socket!(url, bearer, relayHeaders) : deps.socket;
  const url = normalizeDaemonUrl(route.url);
  const http = routeHttp(route, relayToken);
  const options: ClientOptions = {
    ...(legacy ? {} : { expectedEnvironmentId: record.machineId }),
    http,
    ...(socket ? { createSocket: socket, probeAuth: () => probeAuth(url, token, relayHeaders, relay) } : {}),
    ...(relay ? { pageRelay: relayPageOpener(record, route, http.base, deps) } : {}),
  };
  return new KybernClient({ url, token, ...(legacy ? {} : { environmentId: record.machineId }) }, options);
}

function relayPageOpener(record: MachineRecord, route: Route, base: string, deps: MachineClientDeps): PageRelay {
  return async (upstream) => {
    const host = route.via ? deps.relayPages?.(route.via) : undefined;
    if (!host) throw new Error(`Unable to open this page through your network. Reconnect ${record.name}, then try again.`);
    if (!host.hasRelayPages) throw new Error(`Update Kybern on ${host.name} to open pages from ${record.name} through it.`);
    if (!upstream.startsWith(base)) throw new Error("Unable to open this page. Try again.");
    const path = upstream.slice(base.length);
    const via = host.client();
    const opened = await via.call("network.relay_page.open", { machine_id: record.machineId, path });
    // The relay base is `<member>/relay/<id>`: the page lives on the member itself.
    const memberBase = httpBase(route.url).replace(/\/relay\/[^/]+$/, "");
    const page: MachinePage = {
      url: `${memberBase}${opened.path}`,
      scope: `${memberBase}${opened.scope}`,
      close() {
        void via.call("network.relay_page.close", { ticket: opened.ticket }).catch(() => {});
      },
    };
    return page;
  };
}



export class MachineHub {
  readonly set: MachineSet;
  private readonly options: MachineHubOptions;
  private primary: Primary | null = null;
  private readonly watched = new Set<MachineId>();
  private readonly refreshes = new Map<MachineId, boolean>();
  private readonly listeners = new Set<() => void>();
  private flushTimer: unknown;
  private dirty = false;
  private connecting: Promise<void> | undefined;
  private disposed = false;

  constructor(options: MachineHubOptions) {
    this.options = options;
    this.set = new MachineSet({
      store: options.store,
      credentials: options.credentials,
      deviceId: options.deviceId,
      verifyProof: verifyMachineProof,
      cache: options.cache ?? new MachineCache(),
      ...(options.socket ? { socket: options.socket } : {}),
      transport: {
        ...options.transport,
        ...(!options.transport?.nonce && options.randomFill ? { nonce: () => randomNonce(options.randomFill) } : {}),
        createClient: (args) => this.createClient(args),
      },
      ...(options.clock ? { clock: options.clock } : {}),
    });
    this.set.onChange((event) => this.onSetChange(event));
  }

  // ---- the primary: the connection runtime.ts owns ----

  /**
   * `runtime.ts` built a client for the computer the phone is at. It joins the set
   * at once under its saved id (listed offline until it opens), and the set starts
   * connecting every other machine without waiting for it.
   */
  async beginPrimary(client: KybernClient, profile: PrimaryProfile): Promise<void> {
    const primary: Primary = { client, profile, machineId: profile.machineId };
    this.primary = primary;
    await this.set.load();
    if (this.primary !== primary || this.disposed) return;
    const known = this.set.connection(primary.machineId);
    await this.set.adoptConnection(
      {
        machineId: primary.machineId,
        name: profile.name,
        ...(!known && profile.url ? { routes: [{ kind: "direct_advertised" as const, url: routeUrl(profile.url) }] } : {}),
      },
      client,
      { autoRefresh: false },
    );
    if (this.primary !== primary) return;
    this.set.setLocalMachine(primary.machineId);
    this.watchOthers();
    this.connectOthers();
  }

  /** The primary client opened: check the machine id, take on what the daemon says it is, and read the network. */
  async primaryOpened(client: KybernClient): Promise<void> {
    const primary = this.primary;
    if (!primary || primary.client !== client || this.disposed) return;
    const info = client.info;
    const machineId = info?.machine_id || info?.environment_id;
    if (!machineId) return;
    let adoptedAs = primary.machineId;
    if (machineId !== primary.machineId) {
      // Only a stand-in id is replaced; a saved computer that answers as another machine is refused by the client itself.
      if (!isLegacyMachineId(primary.machineId)) return;
      try {
        await this.set.rekeyMachine(primary.machineId, machineId);
        adoptedAs = machineId;
      } catch (error) {
        // The real machine is already listed: this computer is that machine, and the stand-in goes.
        if (error instanceof UnknownMachineError) return;
        await this.set.removeMachine(primary.machineId);
        adoptedAs = machineId;
      }
      if (this.primary !== primary) return;
      const old = primary.machineId;
      primary.machineId = adoptedAs;
      this.options.onRekey?.(old, adoptedAs);
    }
    // A daemon that speaks the network protocol is a member the others can be reached through.
    const member = info?.features?.includes(NETWORK_FEATURES.network) === true;
    await this.set.adoptConnection({ machineId: adoptedAs, name: primary.profile.name, ...(member ? { member: true } : {}) }, client, { autoRefresh: false });
    if (this.primary !== primary) return;
    this.set.setLocalMachine(adoptedAs);
    this.dirty = true;
    this.scheduleFlush();
    this.connectOthers();
  }

  /** The primary client was closed (switching computers, a reconnect from scratch). */
  releasePrimary() {
    if (!this.primary) return;
    this.primary = null;
    this.dirty = false;
    this.cancelFlush();
  }

  /** Where the primary's current lists come from. Called again whenever they change. */
  setPrimaryLists(client: KybernClient, lists: () => PrimaryLists) {
    if (this.primary?.client !== client) return;
    this.primary.lists = lists;
    this.dirty = true;
    this.scheduleFlush();
  }

  /** The lists changed on the primary (a refresh, an event). */
  primaryListsChanged(client: KybernClient) {
    if (this.primary?.client !== client) return;
    this.dirty = true;
    this.scheduleFlush();
  }

  // ---- machines the user adds or removes ----

  /**
   * A machine the phone paired with by code or token: saved at once and connected by the set in the
   * background, so pairing never waits for it to open (it may be minutes away on a bad route).
   */
  async addMachine(record: Pick<MachineRecord, "machineId" | "name" | "routes">, token: string): Promise<void> {
    const created = await this.saveComputer(record, token);
    if (created) void this.set.setPaused(record.machineId, false).then(() => this.watchOthers()).catch(() => {});
  }

  /**
   * A computer saved with its token. A new one is listed paused: it is either about to be the phone's own
   * connection (`beginPrimary` adopts it) or about to be started by `addMachine`, and the set must not dial in between.
   * Returns whether the record is new.
   */
  async saveComputer(record: Pick<MachineRecord, "machineId" | "name" | "routes">, token: string): Promise<boolean> {
    await this.set.load();
    // The token first: nothing connects, and no record points at it, until it is stored.
    await this.options.credentials.set(record.machineId, token);
    if (this.set.connection(record.machineId)) {
      // Known: update its name and address; the set reconnects it in the background if it owns the connection.
      void this.set.addMachine({ machineId: record.machineId, name: record.name, routes: record.routes }).catch(() => {});
      return false;
    }
    await this.set.addMachine({ ...record, member: false, paused: true });
    return true;
  }

  async removeMachine(machineId: MachineId): Promise<void> {
    this.watched.delete(machineId);
    await this.set.removeMachine(machineId);
  }

  async setPaused(machineId: MachineId, paused: boolean): Promise<void> {
    await this.set.setPaused(machineId, paused);
    if (!paused) this.watchOthers();
  }

  /** Try a machine again now (a tap on "Offline"). */
  async retry(machineId: MachineId): Promise<void> {
    if (this.set.connection(machineId)?.record.blockedRoutes?.length) await this.set.clearBlockedRoutes(machineId);
    await this.set.networkChanged();
  }

  // ---- reading ----

  get localMachineId(): MachineId | undefined { return this.set.localMachineId; }
  get primaryClient(): KybernClient | null { return this.primary?.client ?? null; }
  /** The primary's machine id: the one saved, or the daemon's once it answered. */
  get primaryMachineId(): MachineId | undefined { return this.primary?.machineId; }

  /** The machines the set knows (connected or not). */
  get machineCount(): number { return this.set.connections().length; }

  /** The merged lists of every machine. Stable between changes. */
  view(): MergedView { return this.set.merged(); }

  subscribe(listener: () => void): () => void {
    this.listeners.add(listener);
    this.dirty = true;
    this.scheduleFlush();
    return () => { this.listeners.delete(listener); };
  }

  connections(): readonly MachineConnection[] { return this.set.connections(); }

  /** A machine's client (throws `OfflineError` while it cannot be reached). */
  clientFor(machineId: MachineId): KybernClient { return this.set.clientFor(machineId); }

  /** A machine's client while it can be reached, else null (nothing to build a request or open a page on). */
  liveClientFor(machineId: MachineId): KybernClient | null {
    const connection = this.set.connection(machineId);
    if (!connection || !LIVE.has(connection.status)) return null;
    return this.set.clientFor(machineId);
  }

  /**
   * The client of the machine that owns a thread. Thread ids are unique per
   * machine only, so pass `machineId` when the caller has it; an unlisted thread
   * (a new chat) belongs to the machine the user is at.
   */
  clientForThread(threadId: string, machineId?: MachineId): KybernClient {
    return this.set.clientFor(machineId ?? this.set.machineOfThread(threadId) ?? this.requireLocal());
  }

  /** The client of the machine that owns a project instance; `machineId` picks one of several checkouts. */
  clientForProject(projectId: string, machineId?: MachineId): KybernClient {
    return this.set.clientFor(machineId ?? this.set.machineOfProject(projectId) ?? this.requireLocal());
  }

  machineOfThread(threadId: string, hint?: MachineId): MachineId | undefined {
    return this.set.machineOfThread(threadId, hint) ?? this.set.localMachineId;
  }

  /**
   * The machine a request is for when its params name a listed thread, else a listed project.
   * Undefined for one machine, an unlisted id (a new chat) or no id: those go to the machine the user is at.
   */
  routeFor(params: unknown): MachineId | undefined {
    if (this.set.connections().length < 2) return undefined;
    const ids = params as { thread_id?: unknown; project_id?: unknown } | null;
    if (typeof ids?.thread_id === "string") {
      const owner = this.set.machineOfThread(ids.thread_id);
      if (owner) return owner;
    }
    if (typeof ids?.project_id === "string") return this.set.machineOfProject(ids.project_id);
    return undefined;
  }

  machineOfProject(projectId: string, hint?: MachineId): MachineId | undefined {
    return this.set.machineOfProject(projectId, hint) ?? this.set.localMachineId;
  }

  /**
   * A live machine that can manage the network on this phone's behalf: the one the phone is at,
   * else the nearest member. `except` skips a machine (one being removed).
   */
  memberClient(except?: MachineId): KybernClient | undefined {
    const member = this.memberConnection(except);
    return member && this.set.clientFor(member.record.machineId);
  }

  /** The connection of the machine `memberClient` answers through. Never throws: offline machines are simply passed over. */
  memberConnection(except?: MachineId): MachineConnection | undefined {
    const local = this.set.localMachineId;
    return this.set
      .connections()
      .filter(
        (c) =>
          c.record.machineId !== except &&
          LIVE.has(c.status) &&
          c.features.has(NETWORK_FEATURES.network) &&
          (c.record.member || c.record.machineId === local),
      )
      .sort((a, b) => Number(b.record.machineId === local) - Number(a.record.machineId === local) || (a.latencyMs ?? Infinity) - (b.latencyMs ?? Infinity))[0];
  }

  /**
   * Other addresses of the computer the phone is at that answer as it now, besides `current` (the one its client is
   * stuck on): the routes the network reported for it (`network.routes`, learned from a member) and the saved ones.
   */
  async primaryAlternates(current: string): Promise<string[]> {
    const primary = this.primary;
    if (!primary || this.disposed) return [];
    const routes = await this.set.alternateRoutes(primary.machineId, [current]);
    return this.primary === primary ? routes.map((route) => route.url) : [];
  }

  /** Read one machine's lists again (the primary's are read by `runtime.ts`). */
  refreshMachine(id: MachineId): Promise<void> {
    return this.refreshOne(id);
  }

  /** Read every machine the set owns again. The primary is not among them. */
  async refreshOthers(): Promise<void> {
    const ids = this.set.connections().filter((c) => c.record.machineId !== this.primary?.machineId && LIVE.has(c.status)).map((c) => c.record.machineId);
    await Promise.all(ids.map((id) => this.refreshOne(id).catch(() => {})));
  }

  /** The app came back to the foreground: reconnect what dropped and look for better routes. */
  async foreground(): Promise<void> {
    if (!this.disposed) await this.set.networkChanged();
  }

  dispose() {
    this.disposed = true;
    this.cancelFlush();
    this.set.dispose();
    this.listeners.clear();
  }

  // ---- internals ----

  private requireLocal(): MachineId {
    const id = this.set.localMachineId;
    if (!id) throw new UnknownMachineError("local");
    return id;
  }

  private createClient(args: Parameters<MachineTransport["createClient"]>[0]) {
    if (this.options.transport?.createClient) return this.options.transport.createClient(args);
    return createMachineClient(args, {
      ...(this.options.socket ? { socket: this.options.socket } : {}),
      relayPages: (viaId) => {
        const via = this.set.connection(viaId);
        if (!via) return undefined;
        return { name: via.record.name, hasRelayPages: via.features.has(NETWORK_FEATURES.relayPage), client: () => this.set.clientFor(viaId) };
      },
    });
  }

  private onSetChange(event: MachineSetEvent) {
    if (event.type === "records" || event.type === "status") {
      this.watchOthers();
      this.scheduleFlush();
    }
    for (const listener of [...this.listeners]) listener();
  }

  /** Connect every other saved machine, learn the network from a member and read their lists. One run at a time. */
  private connectOthers() {
    if (this.connecting || this.disposed) return;
    this.connecting = this.set.ensureAll().catch(() => {}).finally(() => { this.connecting = undefined; });
  }

  /** Lists changed on a machine the set connects itself: read that machine again (runtime.ts does it for the primary). */
  private watchOthers() {
    for (const connection of this.set.connections()) {
      const id = connection.record.machineId;
      if (this.watched.has(id) || id === this.primary?.machineId) continue;
      this.watched.add(id);
      const handle = this.set.handleFor(id);
      const refresh = () => void this.refreshOne(id).catch(() => {});
      handle.onNotification("projects.changed", refresh);
      handle.onNotification("threads.changed", refresh);
    }
  }

  /** One read per machine at a time; a change that arrives during a read folds into one rerun. */
  private async refreshOne(id: MachineId): Promise<void> {
    if (this.refreshes.has(id)) { this.refreshes.set(id, true); return; }
    this.refreshes.set(id, false);
    try {
      do {
        this.refreshes.set(id, false);
        await this.set.refresh(id);
      } while (this.refreshes.get(id) && !this.disposed);
    } finally {
      this.refreshes.delete(id);
    }
  }

  // The set caches and merges whatever it is given. With one machine the screens read the
  // computer's own lists (`listsOf`), so that work has no reader: the primary's lists wait, by
  // reference, until a second machine is listed.
  private wanted(): boolean {
    return this.set.connections().length > 1;
  }

  private cancelFlush() {
    if (this.flushTimer === undefined) return;
    if (this.options.clearTimer) this.options.clearTimer(this.flushTimer);
    else clearTimeout(this.flushTimer as ReturnType<typeof setTimeout>);
    this.flushTimer = undefined;
  }

  private scheduleFlush() {
    if (this.flushTimer !== undefined || this.disposed || !this.dirty || !this.primary?.lists || !this.wanted()) return;
    const setTimer = this.options.setTimer ?? ((fn: () => void, ms: number) => setTimeout(fn, ms));
    this.flushTimer = setTimer(() => {
      this.flushTimer = undefined;
      const primary = this.primary;
      if (!this.dirty || !primary?.lists) return;
      this.dirty = false;
      const lists = primary.lists();
      this.set.ingest(primary.machineId, { projects: lists.projects, threads: lists.threads, activity: lists.activity });
    }, this.options.ingestDelayMs ?? 300);
  }
}
