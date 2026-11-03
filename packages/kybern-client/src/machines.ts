// One connection per machine, merged by the app (design §3.4 and §5.1). The
// machine set owns each machine's lifecycle: ordered routes with failover,
// identity checks before any credential is sent, relay routes through a
// member, lazily minted per-machine tokens, and each machine's last lists for
// when it is offline. It is framework-agnostic: sockets, HTTP, storage, clocks
// and Ed25519 are all injected, so GPUIX, React Native and the tests share it.
import { httpBase, normalizeDaemonUrl } from "./address.ts";
import { KybernClient, type ClientOptions } from "./client.ts";
import { MachineCache } from "./machineCache.ts";
import { MachineClientProxy, OfflineError, type MachineClient } from "./machineClient.ts";
import {
  machineGlyph,
  machineGlyphs,
  mergeProjects,
  mergeThreads,
  showMachineGlyph,
  type LogicalProject,
  type MachineGlyph,
  type MachineThread,
  type MergedThreads,
} from "./machineMerge.ts";
import { friendlyMachineName } from "./forkMachine.ts";
import {
  IDENTITY_MISMATCH_MESSAGE,
  MAX_ROUTES_PER_MACHINE,
  checkWellKnown,
  orderRoutes,
  probeTimeoutMs,
  relayRoute,
  routeHost,
  routeId,
  routeRefusal,
  wellKnownUrl,
  type ProofInput,
  type WellKnownAnswer,
} from "./machineRoutes.ts";
import {
  NETWORK_FEATURES,
  type DaemonInfo,
  type MachineCard,
  type MachineId,
  type NetworkClientGrantResult,
  type NetworkStatus,
  type Project,
  type Route,
  type Thread,
  type ThreadActivitySummary,
} from "./types.ts";

export { OfflineError } from "./machineClient.ts";
export type { MachineClient } from "./machineClient.ts";

// ---- records, stores, transport ----

/** What a client remembers about one machine. Persisted by the app. */
export interface MachineRecord {
  machineId: MachineId;
  name: string;
  os?: string;
  /** Configured routes, such as a saved SSH tunnel or an advertised URL. */
  routes: Route[];
  /** The machine's Ed25519 key, pinned from the first verified well-known answer or given by the network. */
  signingKey?: string;
  /** In the user's network. A saved daemon older than the network is not a member. */
  member: boolean;
  paused?: boolean;
  legacyProfileId?: string;
  /** Routes the network reported; replaced whole each time it reports. */
  learned?: Route[];
  /** Route ids that answered as another machine. They are never tried again until cleared. */
  blockedRoutes?: string[];
  /** The glyph letters ("S", "Sa") given to this machine; kept so a rename never changes them. */
  glyph?: string;
  /** Tailscale's own name for the machine, as the network last reported it. */
  tsName?: string;
}

type MaybePromise<T> = T | Promise<T>;

export interface MachineStore {
  load(): MaybePromise<MachineRecord[]>;
  save(records: MachineRecord[]): MaybePromise<void>;
}

/** Device tokens, one per machine, in the platform's secure store. */
export interface CredentialStore {
  get(machineId: MachineId): MaybePromise<string | null | undefined>;
  set(machineId: MachineId, token: string): MaybePromise<void>;
  delete(machineId: MachineId): MaybePromise<void>;
  /**
   * Drop a token the machine rejected so `get` stops returning it, without removing a copy an older app
   * still reads. Falls back to `delete` when the store keeps no such copy.
   */
  discard?(machineId: MachineId): MaybePromise<void>;
}

/**
 * Opens the socket of one route. `headers` are extra upgrade headers the platform must send beside
 * `Authorization`: a relay route needs the relay member's own token in `X-Kybern-Relay-Auth`,
 * because `Authorization` is the end-to-end token the target checks (design §3.4).
 */
export type SocketFactory = (url: string, token: string, headers?: Record<string, string>) => WebSocket;

/** The header a relay member reads its caller's token from. */
export const RELAY_AUTH_HEADER = "X-Kybern-Relay-Auth";

export interface Clock {
  now(): number;
  setTimeout(fn: () => void, ms: number): unknown;
  clearTimeout(handle: unknown): void;
}

const systemClock: Clock = {
  now: () => Date.now(),
  setTimeout: (fn, ms) => setTimeout(fn, ms),
  clearTimeout: (handle) => clearTimeout(handle as ReturnType<typeof setTimeout>),
};

/** The effects a machine set needs. Every one has a default except the proof check. */
export interface MachineTransport {
  /** `GET` the identity document at `url`, with no credential. Throws when nothing answers in time. */
  probe(url: string, options: { timeoutMs: number }): Promise<unknown>;
  /**
   * A client that, once `connect()`ed, talks to `route` with `token`. Called only after the route verified.
   * For a relay route `relayToken` is this device's token for the member the route goes through.
   */
  createClient(args: { record: MachineRecord; route: Route; token: string; relayToken?: string }): MachineClient;
  /** 32 random bytes, base64url. */
  nonce(): string;
}

/** `fetch` for the identity document: no redirects, a hard timeout. */
export async function fetchProbe(url: string, options: { timeoutMs: number }): Promise<unknown> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), options.timeoutMs);
  try {
    const response = await fetch(url, { redirect: "error", signal: controller.signal, headers: { accept: "application/json" } });
    if (!response.ok) throw new Error(`The machine answered ${response.status}`);
    return await response.json();
  } finally {
    clearTimeout(timer);
  }
}

const B64URL = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
/** Fills `bytes` with cryptographically random values. */
export type RandomFill = (bytes: Uint8Array<ArrayBuffer>) => void;
const webCryptoFill: RandomFill = (bytes) => {
  // Hermes (React Native) has no Web Crypto: a phone passes its own `fill` (expo-crypto) instead.
  if (!globalThis.crypto?.getRandomValues) throw new Error("This runtime has no secure random source");
  globalThis.crypto.getRandomValues(bytes);
};
/** 32 random bytes, base64url: the identity check's nonce. `fill` defaults to Web Crypto. */
export function randomNonce(fill: RandomFill = webCryptoFill): string {
  const bytes = new Uint8Array(32);
  fill(bytes);
  let out = "";
  for (let at = 0; at < bytes.length; at += 3) {
    const n = (bytes[at]! << 16) | ((bytes[at + 1] ?? 0) << 8) | (bytes[at + 2] ?? 0);
    out += B64URL[(n >> 18) & 63]! + B64URL[(n >> 12) & 63]! + B64URL[(n >> 6) & 63]! + B64URL[n & 63]!;
  }
  return out.slice(0, 43);
}

/** The headers a relay route's upgrade carries for the relay member, or undefined without a token. */
export function relayHeadersFor(relayToken: string | undefined): Record<string, string> | undefined {
  return relayToken ? { [RELAY_AUTH_HEADER]: `Bearer ${relayToken}` } : undefined;
}

/**
 * How a client on `route` reaches the machine's HTTP routes: directly at the route's base, or through
 * the relay member under `/relay/<id>/http` with the member's own token beside the target's.
 */
export function routeHttp(route: Route, relayToken?: string): { base: string; headers?: Record<string, string> } {
  if (route.kind !== "relay") return { base: httpBase(route.url) };
  const headers = relayHeadersFor(relayToken);
  return { base: `${httpBase(route.url)}/http`, ...(headers ? { headers } : {}) };
}

/**
 * Ask the machine whether it accepts `token`, with the `POST /session` a browser socket would make.
 * A native socket refused at the upgrade shows no status, so this is how a revoked token is told
 * from a machine that is merely down. On a relay route a refusal that names the relay is the relay
 * member's own token being refused, which a fresh token for the target would not fix.
 */
export async function probeAuth(
  url: string,
  token: string,
  headers: Record<string, string> | undefined,
  relay: boolean,
  timeoutMs = 5_000,
): Promise<"rejected" | "accepted" | "unknown"> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const response = await fetch(`${httpBase(url)}/session`, {
      method: "POST",
      headers: { ...headers, authorization: `Bearer ${token}` },
      signal: controller.signal,
      redirect: "error",
    });
    if (response.ok) return "accepted";
    if (response.status !== 401 && response.status !== 403) return "unknown";
    if (relay && /relay/i.test(await response.text().catch(() => ""))) return "unknown";
    return "rejected";
  } catch {
    return "unknown";
  } finally {
    clearTimeout(timer);
  }
}

/** The default `createClient`: a `KybernClient` on the route, checking the machine id again on open. */
export function kybernClientFactory(createSocket?: SocketFactory, base: ClientOptions = {}): MachineTransport["createClient"] {
  return ({ record, route, token, relayToken }) => {
    // The relay member authenticates its caller from its own header; the target's token travels in
    // `Authorization` end to end. A browser cannot set either on an upgrade, so a relay route needs a
    // platform socket.
    const relayHeaders = route.kind === "relay" ? relayHeadersFor(relayToken) : undefined;
    if (route.kind === "relay" && (!relayHeaders || !createSocket)) throw new Error("A relay route needs the relay member's token and a native socket");
    const socket: SocketFactory | undefined =
      createSocket && relayHeaders ? (url, bearer) => createSocket(url, bearer, relayHeaders) : createSocket;
    const url = normalizeDaemonUrl(route.url);
    return new KybernClient(
      { url, token, environmentId: record.machineId },
      {
        ...base,
        expectedEnvironmentId: record.machineId,
        http: routeHttp(route, relayToken),
        ...(socket ? { createSocket: socket, probeAuth: () => probeAuth(url, token, relayHeaders, route.kind === "relay") } : {}),
      },
    );
  };
}

// ---- connection view ----

export type MachineConnectionStatus = "online" | "relaying" | "offline" | "paused" | "needs-update" | "unpaired";

export type MachineErrorCode =
  /** An address answered as a different machine. The route is blocked until the user clears it. */
  | "identity_mismatch"
  /** `pinPolicy: "require"` and this machine's key was never pinned. Nothing was sent. */
  | "unpinned"
  /** The identity proof was too old or too new: check the clock. */
  | "clock"
  | "unreachable"
  | "insecure_route"
  | "no_route"
  /** No token for this machine and no member could mint one. */
  | "unpaired"
  | "refresh_failed";

export interface MachineError {
  code: MachineErrorCode;
  message: string;
  route?: Route;
}

export interface MachineConnection {
  record: MachineRecord;
  /** One per machine, for the life of the set; its route changes underneath. Calls reject with `OfflineError` while offline. */
  client: KybernClient;
  status: MachineConnectionStatus;
  info?: DaemonInfo;
  features: Set<string>;
  /** The route in use, while connected. */
  route?: Route;
  /** The member a `relaying` machine goes through. */
  relayVia?: MachineId;
  error?: MachineError;
  /** A connection attempt is running. */
  connecting: boolean;
  latencyMs?: number;
  /** When this machine last answered, in ms. Shown as "Offline · last seen 2 h ago". */
  lastSeenAt?: number;
  /** Cached lists are available for this machine. */
  hasCache: boolean;
}

export type MachineSetEvent = {
  type: "status" | "lists" | "records" | "routes";
  machineId?: MachineId;
};

/** A machine did not send its lists in time. Its cached lists are untouched. */
export class RefreshTimeoutError extends Error {
  readonly machineId: string;
  constructor(machineId: string, machine: string) {
    super(`${machine} did not send its lists in time. Try again`);
    this.name = "RefreshTimeoutError";
    this.machineId = machineId;
  }
}

export class UnknownMachineError extends Error {
  readonly machineId: string;
  constructor(machineId: string) {
    super("This machine is not in your list");
    this.name = "UnknownMachineError";
    this.machineId = machineId;
  }
}

/** The merged lists one `merged()` call returns. Entries that did not change keep their identity. */
export interface MergedView {
  version: number;
  machines: MachineSummary[];
  projects: LogicalProject[];
  threads: MergedThreads;
  /** More than one machine is known: show the machine glyph on threads. */
  showGlyphs: boolean;
}

export interface MachineSummary {
  machineId: MachineId;
  name: string;
  status: MachineConnectionStatus;
  /** The machine's lists are cached copies. */
  offline: boolean;
  lastSeenAt?: number;
  isLocal: boolean;
  glyph: MachineGlyph;
  /** Show the glyph on this machine's threads. */
  showGlyph: boolean;
}

export interface MachineSetOptions {
  store: MachineStore;
  credentials: CredentialStore;
  /** This device's id, sent to `network.client_grant`. Stable across launches. */
  deviceId: string;
  /** Checks an Ed25519 signature. Required: the identity check is the whole point of the pinned key. */
  verifyProof: (input: ProofInput) => MaybePromise<boolean>;
  /** The machine the user is at: the desktop's own daemon, or the member a phone paired with. */
  localMachineId?: MachineId;
  cache?: MachineCache;
  /** Used by the default `createClient`. Native platforms authenticate the upgrade with a header here. */
  socket?: SocketFactory;
  transport?: Partial<MachineTransport>;
  clock?: Clock;
  /** Accept the first key a machine proves as its own when none was given (default). `"require"` refuses to connect to a machine whose key is not pinned. */
  pinPolicy?: "first-use" | "require";
  /** How long a connection may sit reconnecting before the set tries the other routes (default 8 s). */
  reconnectGraceMs?: number;
  /** How long one route may take to open (default 15 s). */
  connectTimeoutMs?: number;
  /** Better routes are probed this often (default 5 min). */
  probeIntervalMs?: number;
  /** How long reading one machine's lists may take before `refresh` gives up on it (default 30 s). */
  refreshTimeoutMs?: number;
  /** Newest threads kept in memory per machine (default 2000). */
  maxThreadsPerMachine?: number;
  /** Machines kept (default 32). */
  maxMachines?: number;
}

const MAX_PROJECTS_PER_MACHINE = 1000;
const FAILURE_DELAYS_MS = [2_000, 4_000, 8_000, 16_000, 32_000, 64_000, 128_000, 300_000];
/**
 * A connect attempt older than this is abandoned when another is asked for: every step of one has its own timeout,
 * so an attempt this old is stuck, and waiting on it would keep the machine offline until the app restarts.
 */
const ATTEMPT_STALE_MS = 120_000;
/** How often a sync may dial a member the daemon saw come back, out of its backoff. */
const REDIAL_MS = 30_000;
/**
 * The longest wait between attempts at a member the daemon the user is at reports online. It answers there, so this
 * device should keep trying, but not with a full route search every sync (a phone off the tailnet, ADE-121).
 */
const ONLINE_RETRY_MAX_MS = 120_000;
/** How long `reach` waits for a machine to connect before it gives up (default). */
const REACH_TIMEOUT_MS = 20_000;
const LIVE: ReadonlySet<MachineConnectionStatus> = new Set(["online", "relaying", "needs-update"]);

interface Opened {
  inner: MachineClient;
  route: Route;
  latencyMs?: number;
  /** The set closes `inner` when it replaces or drops it (default). An app's own connection is not owned. */
  owned?: boolean;
}

/** How an app-owned connection joins the set (`adoptConnection`). */
export interface AdoptOptions {
  /** The set closes the client when it replaces or drops it. Default false: the app opened it and closes it. */
  owned?: boolean;
  /** The route the client is on, for display. Default: the record's first route, else the client's own address. */
  route?: Route;
  /** Lists the app already has; the machine starts with them and they are cached. */
  lists?: { projects?: readonly Project[]; threads?: readonly Thread[]; activity?: readonly ThreadActivitySummary[] };
  /**
   * Read the machine's lists when the connection opens or reopens, and in `refresh()` without an id
   * (default true). Turn it off when the app reads them itself and hands them over with `ingest`; an
   * explicit `refresh(machineId)` still reads.
   */
  autoRefresh?: boolean;
}

interface Slot {
  record: MachineRecord;
  proxy: MachineClientProxy;
  inner?: MachineClient;
  offInner?: () => void;
  route?: Route;
  status: MachineConnectionStatus;
  error?: MachineError;
  latencyMs?: number;
  lastSeenAt?: number;
  /** Bumped to cancel whatever is in flight for this slot. */
  gen: number;
  attempt?: Promise<void>;
  probing?: boolean;
  refreshing?: Promise<void>;
  /** Ids of the routes the last attempt looked at. */
  tried?: Set<string>;
  failures: number;
  retryTimer?: unknown;
  /** When `retryTimer` fires (clock ms). */
  retryAt?: number;
  /** When the attempt in `attempt` started (clock ms). */
  attemptAt?: number;
  /** When a sync last dialed it because the daemon saw it come back (clock ms). */
  redialAt?: number;
  /** Its status in the last member list the daemon reported (`syncMembers`); `undefined` before the first. */
  daemonSaw?: MachineCard["status"];
  graceTimer?: unknown;
  lists?: { projects: Project[]; threads: MachineThread[]; activity?: ThreadActivitySummary[] };
  view?: MachineConnection;
  /** The set closes `inner` when it lets go of it. False for a connection the app owns. */
  owned: boolean;
  /** The app opened this machine's connection: the set never dials, retries or fails it over. */
  external: boolean;
  /** Read the lists on connect and in a whole-set `refresh()`. */
  autoRefresh: boolean;
  /** When this set last minted a token for the machine (clock ms), and how long it waits before the next. */
  mintedAt?: number;
  mintWaitMs?: number;
}

/**
 * A minted token revokes the one this device held there, so mints are limited per machine: at most one a minute, and
 * the wait doubles (to a quarter of an hour) while mints keep following each other, as they would if two windows kept
 * replacing each other's token. A quiet stretch resets it.
 */
export const MINT_MIN_INTERVAL_MS = 60_000;
export const MINT_MAX_INTERVAL_MS = 15 * 60_000;
/** `token` found no saved token but minted one for this machine too recently; the attempt is retried later. */
const MINT_LATER = Symbol("mint later");

/** The `http(s)` base of a daemon address, for showing the route of a connection the app opened. */
function httpUrlOf(url: string): string {
  try {
    const parsed = new URL(url);
    parsed.protocol = parsed.protocol === "wss:" || parsed.protocol === "https:" ? "https:" : "http:";
    return parsed.href.replace(/\/ws\/?$/, "").replace(/\/+$/, "");
  } catch {
    return "http://localhost";
  }
}

/** An address as one comparable `http(s)://host:port` base, whether it was written as `ws://…/ws` or `http://…`. */
function baseUrl(url: string): string {
  try {
    return httpBase(url).replace(/\/+$/, "");
  } catch {
    return url;
  }
}

/** A new token for the machine was minted a moment ago; the next attempt (after the usual retry wait) tries again. */
function mintLaterError(machine: string): MachineError {
  return { code: "unreachable", message: `${machine} did not accept this device's sign-in. Kybern will try again in a minute` };
}

function errorMessage(code: MachineErrorCode, machine: string): string {
  switch (code) {
    case "identity_mismatch":
      return IDENTITY_MISMATCH_MESSAGE;
    case "unpinned":
      return `${machine} has not been verified yet. Add it again from a machine that is in your network`;
    case "clock":
      return `${machine} answered with a time that is off. Check the date and time on both`;
    case "insecure_route":
      return `Public addresses must use HTTPS. Add a secure address for ${machine}`;
    case "no_route":
      return `No address for ${machine} yet`;
    case "unpaired":
      return `Pair this device with ${machine}`;
    case "refresh_failed":
      return `Could not read ${machine}'s lists`;
    default:
      return `${machine} is not answering`;
  }
}

const ERROR_RANK: Record<MachineErrorCode, number> = {
  identity_mismatch: 7,
  unpinned: 6,
  clock: 5,
  insecure_route: 4,
  unreachable: 3,
  no_route: 2,
  unpaired: 1,
  refresh_failed: 0,
};

async function mapLimit<T>(items: readonly T[], limit: number, fn: (item: T) => Promise<void>): Promise<void> {
  let next = 0;
  const worker = async () => {
    while (next < items.length) await fn(items[next++]!);
  };
  await Promise.all(Array.from({ length: Math.min(limit, items.length) }, worker));
}

export class MachineSet {
  private readonly options: MachineSetOptions;
  private readonly transport: MachineTransport;
  private readonly clock: Clock;
  private readonly cache: MachineCache;
  private readonly slots = new Map<MachineId, Slot>();
  private readonly listeners = new Set<(event: MachineSetEvent) => void>();
  private localId: MachineId | undefined;
  private loaded: Promise<void> | undefined;
  private saving: Promise<void> = Promise.resolve();
  private saveQueued = false;
  private probeTimer: unknown;
  private syncTimer: unknown;
  private syncing: Promise<void> | undefined;
  /** The status the last `network.changed` carried, and the member that sent it, until a sync uses it. */
  private pushed: { slot: Slot; status: NetworkStatus } | undefined;
  private syncAgain = false;
  private disposed = false;
  private _version = 0;
  private mergedView: MergedView | undefined;
  private owners: { version: number; threads: Map<string, MachineId[]>; projects: Map<string, MachineId[]> } | undefined;

  constructor(options: MachineSetOptions) {
    this.options = options;
    this.clock = options.clock ?? systemClock;
    this.cache = options.cache ?? new MachineCache();
    this.localId = options.localMachineId;
    this.transport = {
      probe: options.transport?.probe ?? fetchProbe,
      createClient: options.transport?.createClient ?? kybernClientFactory(options.socket),
      nonce: options.transport?.nonce ?? randomNonce,
    };
  }

  // ---- reading ----

  get version(): number {
    return this._version;
  }

  /** The machine the user is at. */
  get localMachineId(): MachineId | undefined {
    return this.localId;
  }

  setLocalMachine(machineId: MachineId | undefined): void {
    if (this.localId === machineId) return;
    this.localId = machineId;
    this.touch(undefined, "records");
  }

  /** Every machine, the local one first, then in the order they were added. */
  connections(): readonly MachineConnection[] {
    return this.orderedSlots().map((slot) => this.viewOf(slot));
  }

  connection(machineId: MachineId): MachineConnection | undefined {
    const slot = this.slots.get(machineId);
    return slot && this.viewOf(slot);
  }

  /** The machine's client. Throws `OfflineError` (with `cached`) when it cannot be reached now. */
  clientFor(machineId: MachineId): KybernClient {
    const slot = this.slots.get(machineId);
    if (!slot) throw new UnknownMachineError(machineId);
    if (!slot.inner || !LIVE.has(slot.status)) throw new OfflineError(machineId, this.hasCache(slot));
    return slot.proxy;
  }

  /**
   * The machine's client even while it is offline. Subscriptions and handlers
   * made on it are kept and attached when the machine connects; calls reject
   * with `OfflineError` until then.
   */
  handleFor(machineId: MachineId): KybernClient {
    const slot = this.slots.get(machineId);
    if (!slot) throw new UnknownMachineError(machineId);
    return slot.proxy;
  }

  /**
   * The machine that owns a thread. Thread ids are unique per machine, not
   * across machines: when two machines list the same id, `hint` decides, then
   * the local machine, then the first machine in order. Use `scopedKey` to
   * avoid the question.
   */
  machineOfThread(id: string, hint?: MachineId): MachineId | undefined {
    return this.pick(this.ownerIndex().threads.get(id), hint);
  }

  machineOfProject(id: string, hint?: MachineId): MachineId | undefined {
    return this.pick(this.ownerIndex().projects.get(id), hint);
  }

  /** Every machine that lists this thread id. */
  machinesOfThread(id: string): MachineId[] {
    return [...(this.ownerIndex().threads.get(id) ?? [])];
  }

  onChange(fn: (event: MachineSetEvent) => void): () => void {
    this.listeners.add(fn);
    return () => {
      this.listeners.delete(fn);
    };
  }

  // ---- lifecycle ----

  /** Load saved machines and lists, connect to every machine, learn the rest of the network from any member, and read each machine's lists. */
  async ensureAll(): Promise<void> {
    await this.load();
    if (this.disposed) return;
    await this.connectAll();
    await this.syncMembers();
    // A machine that had no token may be mintable now that a member is up.
    await mapLimit(
      [...this.slots.values()].filter((slot) => slot.status === "unpaired" && !slot.record.paused),
      4,
      (slot) => this.connectSlot(slot),
    );
    await this.refresh();
    this.scheduleProbe();
  }

  /** Add a machine the user just paired with, keeping its token, and connect to it. */
  async addMachine(
    input: Pick<MachineRecord, "machineId" | "name"> & Partial<Omit<MachineRecord, "machineId" | "name">>,
    token?: string,
  ): Promise<MachineConnection> {
    await this.load();
    const existing = this.slots.get(input.machineId);
    if (token) await this.options.credentials.set(input.machineId, token);
    if (existing) {
      Object.assign(existing.record, input, { routes: input.routes ?? existing.record.routes });
      this.persist();
      this.touch(existing, "records");
      await this.connectSlot(existing);
      return this.viewOf(existing);
    }
    if (this.slots.size >= (this.options.maxMachines ?? 32)) throw new Error("Remove a machine before adding another");
    const slot = this.createSlot({ routes: [], member: true, ...input });
    this.persist();
    this.touch(slot, "records");
    await this.connectSlot(slot);
    return this.viewOf(slot);
  }

  /**
   * Add a machine whose connection the app already holds (its primary socket) and make it the machine's
   * connection. The set uses it as is: it never dials, retries or fails it over, and unless `owned` it never
   * closes it. Adopting a machine that is already listed merges the record and replaces its connection;
   * adopt again when the app opens a new client.
   */
  async adoptConnection(
    input: Pick<MachineRecord, "machineId" | "name"> & Partial<Omit<MachineRecord, "machineId" | "name">>,
    client: MachineClient,
    options: AdoptOptions = {},
  ): Promise<MachineConnection> {
    await this.load();
    if (this.disposed) throw new Error("The machine set was closed");
    let slot = this.slots.get(input.machineId);
    if (slot) {
      Object.assign(slot.record, input, { routes: input.routes ?? slot.record.routes });
    } else {
      if (this.slots.size >= (this.options.maxMachines ?? 32)) throw new Error("Remove a machine before adding another");
      slot = this.createSlot({ routes: [], member: false, ...input });
    }
    slot.record.paused = false;
    slot.gen++;
    slot.attempt = undefined;
    slot.external = true;
    slot.autoRefresh = options.autoRefresh ?? true;
    this.persist();
    this.adopt(slot, {
      inner: client,
      route: options.route ?? slot.record.routes[0] ?? { kind: "direct_advertised", url: httpUrlOf(client.endpoint.url) },
      owned: options.owned ?? false,
    });
    if (options.lists) this.ingest(input.machineId, options.lists);
    return this.viewOf(slot);
  }

  /**
   * A machine has a new id (a stand-in became the daemon's real one). Its record, cached lists, connection and
   * token move to `newId` in one step with one change event, and it keeps its place in the order. Rejects
   * when `oldId` is unknown or `newId` is already listed; nothing changes then.
   */
  async rekeyMachine(oldId: MachineId, newId: MachineId): Promise<MachineConnection> {
    await this.load();
    const slot = this.slots.get(oldId);
    if (!slot) throw new UnknownMachineError(oldId);
    if (oldId === newId) return this.viewOf(slot);
    const taken = () => new Error(`${newId} is already in your list. Remove it first`);
    if (this.slots.has(newId)) throw taken();
    // The token goes first: if it cannot be saved under the new id nothing else has moved.
    let token: string | null | undefined;
    try {
      token = await this.options.credentials.get(oldId);
    } catch {
      token = undefined;
    }
    if (token) await this.options.credentials.set(newId, token);
    if (this.slots.get(oldId) !== slot || this.slots.has(newId)) {
      if (token && !this.slots.has(newId)) await Promise.resolve(this.options.credentials.delete(newId)).catch(() => {});
      if (this.slots.get(oldId) !== slot) throw new UnknownMachineError(oldId);
      throw taken();
    }
    // From here on nothing awaits, so no reader sees the machine half moved.
    const entries = [...this.slots.entries()].map(([id, other]): [MachineId, Slot] => [other === slot ? newId : id, other]);
    this.slots.clear();
    for (const [id, other] of entries) this.slots.set(id, other);
    slot.record.machineId = newId;
    slot.proxy.rename(newId);
    const snapshot = this.cache.get(oldId);
    if (snapshot) {
      this.cache.delete(oldId);
      this.cache.put(newId, { projects: snapshot.projects, threads: snapshot.threads }, snapshot.savedAt);
    }
    if (this.localId === oldId) this.localId = newId;
    this.persist();
    this.touch(slot, "records");
    if (token) {
      try {
        // The copy under the stand-in id only; a store that keeps the older app's copy leaves it (see `discard`).
        const { credentials } = this.options;
        await (credentials.discard ? credentials.discard(oldId) : credentials.delete(oldId));
      } catch {
        // The old id has no record any more; its token is never used.
      }
    }
    return this.viewOf(slot);
  }

  /**
   * Discovery learned a machine before its saved profile answered with that id. Keep the
   * canonical member's proxy and trust pin, and move the saved profile's routes and metadata
   * onto it. Discard only the obsolete machine-id credential, never the saved profile token.
   */
  async mergeSavedMachine(oldId: MachineId, newId: MachineId): Promise<MachineConnection> {
    const saved = this.slots.get(oldId);
    const member = this.slots.get(newId);
    if (!saved || !member) throw new UnknownMachineError(!saved ? oldId : newId);
    if (saved === member) return this.viewOf(member);
    const { credentials } = this.options;
    const token = await credentials.get(oldId);
    const currentToken = await credentials.get(newId);
    if (this.slots.get(oldId) !== saved || this.slots.get(newId) !== member) {
      if (!this.slots.has(oldId) && this.slots.get(newId) === member) return this.viewOf(member);
      throw new UnknownMachineError(oldId);
    }
    if (!currentToken && token) await credentials.set(newId, token);
    if (this.slots.get(oldId) !== saved || this.slots.get(newId) !== member) throw new UnknownMachineError(oldId);
    member.record = {
      ...saved.record, ...member.record,
      routes: orderRoutes(saved.record.routes, member.record.routes).slice(0, MAX_ROUTES_PER_MACHINE),
      learned: orderRoutes(saved.record.learned, member.record.learned).slice(0, MAX_ROUTES_PER_MACHINE),
      blockedRoutes: [...new Set([...(saved.record.blockedRoutes ?? []), ...(member.record.blockedRoutes ?? [])])].slice(-MAX_ROUTES_PER_MACHINE),
      ...(saved.record.legacyProfileId ? { legacyProfileId: saved.record.legacyProfileId } : {}),
    };
    const cached = this.cache.get(oldId);
    if (cached && !this.cache.get(newId)) this.cache.put(newId, { projects: cached.projects, threads: cached.threads }, cached.savedAt);
    member.lists ??= saved.lists;
    this.cache.delete(oldId);
    this.dropSlot(saved);
    this.slots.delete(oldId);
    if (this.localId === oldId) this.localId = newId;
    this.persist();
    this.touch(member, "records");
    // A desktop store with legacy profiles supplies discard. Other stores have only
    // machine-id credentials, so deleting the old id cannot delete a profile account.
    await (credentials.discard ? credentials.discard(oldId) : credentials.delete(oldId));
    return this.viewOf(member);
  }

  /** Forget a machine: close it and delete its token, cached lists and record. */
  async removeMachine(machineId: MachineId): Promise<void> {
    const slot = this.slots.get(machineId);
    if (!slot) return;
    this.dropSlot(slot);
    this.slots.delete(machineId);
    this.cache.delete(machineId);
    try {
      await this.options.credentials.delete(machineId);
    } catch {
      // The record is gone; a leftover token is never used without one.
    }
    this.persist();
    this.touch(undefined, "records");
  }

  /** Pause: keep the machine listed from its cache and stop talking to it. */
  async setPaused(machineId: MachineId, paused: boolean): Promise<void> {
    const slot = this.slots.get(machineId);
    if (!slot || !!slot.record.paused === paused) return;
    slot.record.paused = paused;
    this.persist();
    if (paused) this.goOffline(slot, "paused");
    else if (slot.external) {
      // Only the app can reopen its own connection: it adopts a new client when it has one.
      slot.status = "offline";
      this.touch(slot, "status");
    } else await this.connectSlot(slot);
    this.touch(slot, "records");
  }

  /** The user chose to trust a blocked address again (or one machine's blocks entirely). */
  async clearBlockedRoutes(machineId: MachineId, route?: Route): Promise<void> {
    const slot = this.slots.get(machineId);
    if (!slot) return;
    slot.record.blockedRoutes = route ? (slot.record.blockedRoutes ?? []).filter((id) => id !== routeId(route)) : [];
    this.persist();
    await this.connectSlot(slot);
  }

  /** The OS reported a network change: retry offline machines now and look for better routes. */
  async networkChanged(): Promise<void> {
    for (const slot of this.slots.values()) {
      slot.failures = 0;
      this.cancelRetry(slot);
    }
    await Promise.all(
      [...this.slots.values()].map((slot) => (LIVE.has(slot.status) ? this.probeBetter(slot) : this.connectSlot(slot))),
    );
  }

  /**
   * Connect to a machine now, out of its backoff, and wait until it is up: what the app does when the user opens
   * something that lives there. Resolves true when the machine is connected (at once if it already is), false when
   * it is paused, opened by the app itself, or still not connected after `timeoutMs`.
   */
  async reach(machineId: MachineId, timeoutMs = REACH_TIMEOUT_MS): Promise<boolean> {
    await this.load();
    const slot = this.slots.get(machineId);
    if (!slot) throw new UnknownMachineError(machineId);
    const live = () => !!slot.inner && LIVE.has(slot.status) && this.slots.get(slot.record.machineId) === slot;
    if (live()) return true;
    if (slot.external || slot.record.paused || this.disposed) return false;
    let timer: unknown;
    let timedOut = false;
    const waited = new Promise<void>((resolve) => {
      timer = this.clock.setTimeout(() => {
        timedOut = true;
        resolve();
      }, timeoutMs);
    });
    try {
      // The attempt under way (or a new one), then one more if that one failed: the first can be an attempt that
      // started before the machine came back. A machine that is up sooner ends the wait at once.
      for (let round = 0; round < 2 && !live() && !timedOut && !this.disposed && !slot.record.paused; round++) {
        slot.failures = 0;
        await Promise.race([this.connectSlot(slot), waited]);
      }
    } finally {
      this.clock.clearTimeout(timer);
    }
    return live();
  }

  /**
   * The direct routes of a machine, other than the addresses in `except`, that answer as that machine now, in the order
   * they should be tried. For a connection the app owns (`adoptConnection`) whose saved address stopped reaching the
   * machine (its port moved, or an old daemon still holds the address and refuses sessions): the app reopens it on one of
   * these. A member's routes pass the identity check before they are returned, so no credential goes to an address that
   * did not prove it is the machine. Relay and SSH routes are left out: the app's own client cannot use them.
   */
  async alternateRoutes(machineId: MachineId, except: readonly string[] = []): Promise<Route[]> {
    await this.load();
    const slot = this.slots.get(machineId);
    if (!slot || this.disposed) return [];
    const skip = new Set(except.map((url) => baseUrl(url)));
    const routes = orderRoutes(slot.record.routes, slot.record.learned).filter(
      (route) =>
        (route.kind === "direct_tailnet" || route.kind === "direct_advertised") &&
        !skip.has(baseUrl(route.url)) &&
        !slot.record.blockedRoutes?.includes(routeId(route)) &&
        !routeRefusal(route),
    );
    const verified: Route[] = [];
    for (const route of routes) {
      if (this.disposed || this.slots.get(machineId) !== slot) return [];
      if ((await this.verifyRoute(slot, route)).ok) verified.push(route);
    }
    return verified;
  }

  /** Close every connection and stop every timer. The set cannot be used afterwards. */
  dispose(): void {
    this.disposed = true;
    this.clock.clearTimeout(this.probeTimer);
    this.clock.clearTimeout(this.syncTimer);
    for (const slot of this.slots.values()) this.dropSlot(slot);
    this.listeners.clear();
  }

  // ---- lists ----

  /** Replace what is known of a machine's lists with a read the app made itself, and cache it. */
  ingest(
    machineId: MachineId,
    lists: { projects?: readonly Project[]; threads?: readonly Thread[]; activity?: readonly ThreadActivitySummary[] },
  ): void {
    const slot = this.slots.get(machineId);
    if (!slot) return;
    // Bounded in memory: the newest threads only, however many a machine has.
    const max = this.options.maxThreadsPerMachine ?? 2000;
    const projects = lists.projects ? lists.projects.slice(0, MAX_PROJECTS_PER_MACHINE) : (slot.lists?.projects ?? []);
    const threads = lists.threads
      ? lists.threads.length > max
        ? [...lists.threads].sort((a, b) => Date.parse(b.updated_at) - Date.parse(a.updated_at)).slice(0, max)
        : [...lists.threads]
      : (slot.lists?.threads ?? []);
    // The activity belongs to the threads it was read with: replaced with them, kept when only projects change.
    const activity = lists.threads ? lists.activity : slot.lists?.activity;
    slot.lists = { projects, threads, ...(activity ? { activity: [...activity] } : {}) };
    if (LIVE.has(slot.status)) this.cache.put(machineId, { projects, threads }, this.clock.now());
    this.touch(slot, "lists");
  }

  /**
   * A live event changed one thread of a machine (`thread_created`, `thread_updated`): replace its row in the
   * machine's list, or add it, without another `threads.list`. An archived thread leaves the list. Stale events
   * (an older `last_seq` than the listed row) and machines without lists yet change nothing. Not cached: the
   * next list read is.
   */
  patchThread(machineId: MachineId, thread: Thread): boolean {
    const slot = this.slots.get(machineId);
    if (!slot?.lists) return false;
    const threads = slot.lists.threads;
    const index = threads.findIndex((t) => t.id === thread.id);
    if (index >= 0 && threads[index]!.last_seq > thread.last_seq) return false;
    if (thread.status === "archived") return this.removeThread(machineId, thread.id);
    const next = index >= 0 ? threads.map((t, at) => (at === index ? thread : t)) : [thread, ...threads];
    slot.lists = { ...slot.lists, threads: next };
    this.touch(slot, "lists");
    return true;
  }

  /** `thread_archived` on a machine: the thread leaves its list. */
  removeThread(machineId: MachineId, threadId: string): boolean {
    const slot = this.slots.get(machineId);
    if (!slot?.lists || !slot.lists.threads.some((t) => t.id === threadId)) return false;
    slot.lists = { ...slot.lists, threads: slot.lists.threads.filter((t) => t.id !== threadId) };
    this.touch(slot, "lists");
    return true;
  }

  /** What `threads.list` reported as live activity for a machine, as of its last read. */
  activityOf(machineId: MachineId): readonly ThreadActivitySummary[] | undefined {
    return this.slots.get(machineId)?.lists?.activity;
  }

  /**
   * Read projects and threads from every machine that is online (or one machine) and replace its lists and
   * cached copy. Reading one machine rejects with `RefreshTimeoutError` when its lists do not arrive in time,
   * leaving what it had; reading all of them never rejects.
   */
  async refresh(machineId?: MachineId): Promise<void> {
    const targets = [...this.slots.values()].filter(
      (slot) => (machineId ? slot.record.machineId === machineId : slot.autoRefresh) && slot.inner && LIVE.has(slot.status),
    );
    if (machineId) await Promise.all(targets.map((slot) => this.refreshSlot(slot)));
    else await mapLimit(targets, 4, (slot) => this.refreshSlot(slot).catch(() => {}));
  }

  private refreshQuietly(machineId: MachineId): void {
    this.refresh(machineId).catch(() => {});
  }

  /** One read per machine at a time; a second caller waits for the first. */
  private refreshSlot(slot: Slot): Promise<void> {
    if (slot.refreshing) return slot.refreshing;
    const inner = slot.inner;
    if (!inner) return Promise.resolve();
    const run = (async () => {
      let timer: unknown;
      let timedOut = false;
      try {
        const reads = Promise.all([inner.call("projects.list", {}), inner.call("threads.list", {})]);
        reads.catch(() => {});
        const timeout = new Promise<never>((_, reject) => {
          timer = this.clock.setTimeout(() => {
            timedOut = true;
            reject(new RefreshTimeoutError(slot.record.machineId, slot.record.name));
          }, this.options.refreshTimeoutMs ?? 30_000);
        });
        const [projects, threads] = await Promise.race([reads, timeout]);
        if (slot.inner !== inner) return;
        this.ingest(slot.record.machineId, {
          projects: projects.projects,
          threads: threads.threads,
          activity: threads.activity,
        });
      } catch (error) {
        if (slot.inner === inner && !slot.error) {
          slot.error = { code: "refresh_failed", message: errorMessage("refresh_failed", slot.record.name) };
          this.touch(slot, "status");
        }
        if (timedOut && slot.inner === inner) throw error;
      } finally {
        this.clock.clearTimeout(timer);
      }
    })().finally(() => {
      if (slot.refreshing === run) slot.refreshing = undefined;
    });
    slot.refreshing = run;
    return run;
  }

  /** The merged projects and threads of every machine. Cached until something changes. */
  merged(): MergedView {
    if (this.mergedView?.version === this._version) return this.mergedView;
    const slots = this.orderedSlots();
    const offline = new Set<MachineId>();
    const lastSeenAt = new Map<MachineId, number>();
    const projects = new Map<MachineId, readonly Project[]>();
    const threads = new Map<MachineId, readonly MachineThread[]>();
    for (const slot of slots) {
      const id = slot.record.machineId;
      if (!LIVE.has(slot.status)) {
        offline.add(id);
        const seen = slot.lastSeenAt ?? this.cache.get(id)?.savedAt;
        if (seen !== undefined) lastSeenAt.set(id, seen);
      }
      if (slot.lists) {
        projects.set(id, slot.lists.projects);
        threads.set(id, slot.lists.threads);
      }
    }
    const machineOrder = slots.map((slot) => slot.record.machineId);
    const logical = mergeProjects(projects, {
      localMachineId: this.localId,
      machineOrder,
      offline,
      previous: this.mergedView?.projects,
    });
    const merged = mergeThreads(threads, logical, {
      offline,
      lastSeenAt,
      previous: this.mergedView?.threads,
      maxPerMachine: this.options.maxThreadsPerMachine ?? 2000,
    });
    const count = slots.length;
    const glyphs = this.glyphs();
    this.mergedView = {
      version: this._version,
      projects: logical,
      threads: merged,
      showGlyphs: count > 1,
      machines: slots.map((slot) => {
        const machineId = slot.record.machineId;
        return {
          machineId,
          // The name people use: a host name loses `.local` and its tailnet suffix ("Treshnandas-Macbook.local").
          name: friendlyMachineName(slot.record.name, slot.record.tsName) || slot.record.name,
          status: slot.status,
          offline: offline.has(machineId),
          lastSeenAt: lastSeenAt.get(machineId),
          isLocal: machineId === this.localId,
          glyph: glyphs.get(machineId) ?? machineGlyph({ machineId, name: slot.record.name }),
          showGlyph: showMachineGlyph({ machineCount: count, machineId, localMachineId: this.localId }),
        };
      }),
    };
    return this.mergedView;
  }

  /**
   * Every machine's glyph. A machine keeps the letter it was first given (saved in its record), so renaming
   * a machine or adding another never reshuffles the badges; a new machine takes the first free candidate of its name.
   */
  glyphs(): Map<MachineId, MachineGlyph> {
    const slots = [...this.slots.values()];
    const assigned = new Map<MachineId, string>();
    for (const slot of slots) if (slot.record.glyph) assigned.set(slot.record.machineId, slot.record.glyph);
    const glyphs = machineGlyphs(slots.map((slot) => ({ machineId: slot.record.machineId, name: friendlyMachineName(slot.record.name, slot.record.tsName) || slot.record.name })), assigned);
    if (glyphs.fresh.size) {
      for (const [id, letter] of glyphs.fresh) this.slots.get(id)!.record.glyph = letter;
      this.persist();
    }
    return glyphs;
  }

  /** What a machine is called on screen (`friendlyMachineName`), or the id of a machine this set does not know. */
  displayName(machineId: MachineId): string {
    const record = this.slots.get(machineId)?.record;
    return record ? friendlyMachineName(record.name, record.tsName) || record.name : machineId;
  }

  // ---- loading and persistence ----

  /** Load the saved machines and their cached lists (once). Every other call that needs them does this itself. */
  load(): Promise<void> {
    this.loaded ??= (async () => {
      let records: MachineRecord[] = [];
      try {
        records = (await this.options.store.load()) ?? [];
      } catch {
        records = [];
      }
      for (const record of records.slice(0, this.options.maxMachines ?? 32)) {
        if (!record?.machineId || this.slots.has(record.machineId)) continue;
        this.createSlot({ ...record, routes: [...(record.routes ?? [])] });
      }
      await this.cache.load();
      for (const slot of this.slots.values()) {
        const snapshot = this.cache.get(slot.record.machineId);
        if (snapshot && !slot.lists) {
          slot.lists = { projects: snapshot.projects, threads: snapshot.threads };
          slot.lastSeenAt ??= snapshot.savedAt;
        }
      }
      this.touch(undefined, "records");
    })();
    return this.loaded;
  }

  private persist() {
    if (this.saveQueued) return;
    this.saveQueued = true;
    this.saving = this.saving.then(async () => {
      this.saveQueued = false;
      try {
        await this.options.store.save([...this.slots.values()].map((slot) => slot.record));
      } catch {
        // The next change saves again.
      }
    });
  }

  /** Resolves when everything changed so far is saved. */
  async flush(): Promise<void> {
    await this.saving;
    await this.cache.flush();
  }

  private createSlot(record: MachineRecord): Slot {
    const slot: Slot = {
      record,
      proxy: new MachineClientProxy(record.machineId, () => this.hasCache(slot), () => this.displayName(slot.record.machineId)),
      status: record.paused ? "paused" : "offline",
      gen: 0,
      failures: 0,
      owned: true,
      external: false,
      autoRefresh: true,
    };
    // Members tell clients when the network changes. The proxy keeps these handlers across route changes.
    slot.proxy.onNotification("network.changed", (params) => this.scheduleSync(slot, params));
    slot.proxy.onNotification("network.machine.changed", () => this.scheduleSync());
    this.slots.set(record.machineId, slot);
    return slot;
  }

  private dropSlot(slot: Slot) {
    slot.gen++;
    this.cancelRetry(slot);
    this.clock.clearTimeout(slot.graceTimer);
    slot.offInner?.();
    slot.offInner = undefined;
    slot.proxy.attach(undefined);
    if (slot.owned) slot.inner?.close("Machine removed");
    slot.inner = undefined;
  }

  private hasCache(slot: Slot): boolean {
    return !!slot.lists || !!this.cache.get(slot.record.machineId);
  }

  private orderedSlots(): Slot[] {
    const all = [...this.slots.values()];
    const local = all.find((slot) => slot.record.machineId === this.localId);
    return local ? [local, ...all.filter((slot) => slot !== local)] : all;
  }

  private pick(owners: readonly MachineId[] | undefined, hint: MachineId | undefined): MachineId | undefined {
    if (!owners?.length) return undefined;
    if (hint && owners.includes(hint)) return hint;
    if (this.localId && owners.includes(this.localId)) return this.localId;
    return owners[0];
  }

  private ownerIndex() {
    if (this.owners?.version === this._version) return this.owners;
    const threads = new Map<string, MachineId[]>();
    const projects = new Map<string, MachineId[]>();
    const add = (index: Map<string, MachineId[]>, id: string, machineId: MachineId) => {
      const list = index.get(id);
      if (!list) index.set(id, [machineId]);
      else if (!list.includes(machineId)) list.push(machineId);
    };
    for (const slot of this.orderedSlots()) {
      for (const thread of slot.lists?.threads ?? []) add(threads, thread.id, slot.record.machineId);
      for (const project of slot.lists?.projects ?? []) add(projects, project.id, slot.record.machineId);
    }
    this.owners = { version: this._version, threads, projects };
    return this.owners;
  }

  private touch(slot: Slot | undefined, type: MachineSetEvent["type"]) {
    if (slot) slot.view = undefined;
    this._version++;
    const event: MachineSetEvent = slot ? { type, machineId: slot.record.machineId } : { type };
    for (const fn of [...this.listeners]) fn(event);
  }

  private viewOf(slot: Slot): MachineConnection {
    if (slot.view) return slot.view;
    const info = slot.inner?.info ?? undefined;
    const view: MachineConnection = {
      record: slot.record,
      client: slot.proxy,
      status: slot.status,
      features: new Set(info?.features ?? []),
      connecting: slot.attempt !== undefined,
      hasCache: this.hasCache(slot),
    };
    if (info) view.info = info;
    if (slot.route && LIVE.has(slot.status)) {
      view.route = slot.route;
      if (slot.route.kind === "relay" && slot.route.via) view.relayVia = slot.route.via;
    }
    if (slot.error) view.error = slot.error;
    if (slot.latencyMs !== undefined) view.latencyMs = slot.latencyMs;
    if (slot.lastSeenAt !== undefined) view.lastSeenAt = slot.lastSeenAt;
    slot.view = view;
    return view;
  }

  // ---- connecting ----

  private async connectAll(): Promise<void> {
    const todo = [...this.slots.values()].filter((slot) => !slot.record.paused && !LIVE.has(slot.status));
    await mapLimit(todo, 4, (slot) => this.connectSlot(slot));
    // A machine only reachable through a member waits for that member to be up.
    // Only machines that now have a relay route they have not tried; the direct routes that failed are not probed again.
    const waiting = todo.filter(
      (slot) =>
        slot.record.member &&
        slot.status === "offline" &&
        this.candidates(slot).some((route) => !slot.tried?.has(routeId(route))),
    );
    if (waiting.length) await mapLimit(waiting, 4, (slot) => this.connectSlot(slot));
  }

  /** Find a route that verifies and opens, and move the machine onto it. Concurrent calls share one attempt. */
  private connectSlot(slot: Slot): Promise<void> {
    if (slot.record.paused) {
      this.goOffline(slot, "paused");
      return Promise.resolve();
    }
    if (slot.external) return Promise.resolve();
    if (slot.attempt && this.clock.now() - (slot.attemptAt ?? 0) < ATTEMPT_STALE_MS) return slot.attempt;
    // A stuck attempt is superseded: the new generation makes whatever it still finds close itself.
    this.cancelRetry(slot);
    const gen = ++slot.gen;
    slot.attemptAt = this.clock.now();
    const attempt = (async () => {
      try {
        const result = await this.findRoute(slot, gen, {});
        if (gen !== slot.gen || this.disposed) {
          if (result.kind === "open") result.opened.inner.close("Superseded");
          return;
        }
        if (result.kind === "open") this.adopt(slot, result.opened);
        else this.goOffline(slot, result.kind === "unpaired" ? "unpaired" : "offline", result.error);
      } catch (error) {
        // Never offline without a reason: the row shows what went wrong, so a broken attempt is not a silent "Offline".
        if (gen === slot.gen) {
          const detail = error instanceof Error && error.message ? error.message : String(error);
          this.goOffline(slot, "offline", { code: "unreachable", message: `Unable to connect to ${slot.record.name}: ${detail}` });
        }
      }
    })().finally(() => {
      if (slot.attempt === attempt) slot.attempt = undefined;
      if (gen === slot.gen) this.touch(slot, "status");
    });
    slot.attempt = attempt;
    this.touch(slot, "status");
    return attempt;
  }

  /** Candidate routes in the order they are tried: tailnet, advertised, SSH, then relays through connected members. */
  private candidates(slot: Slot): Route[] {
    const { record } = slot;
    const own = orderRoutes(record.routes, record.learned);
    if (!record.member) return own;
    const relays: Route[] = [];
    const members = [...this.slots.values()]
      .filter(
        (other) =>
          other !== slot &&
          other.record.member &&
          other.route &&
          other.route.kind !== "relay" &&
          (other.status === "online" || other.status === "needs-update") &&
          other.inner?.info?.features?.includes(NETWORK_FEATURES.relay) !== false,
      )
      .sort((a, b) => (a.latencyMs ?? Infinity) - (b.latencyMs ?? Infinity));
    for (const member of members) relays.push(relayRoute(member.route!.url, member.record.machineId, record.machineId));
    return orderRoutes(own, relays);
  }

  private async findRoute(
    slot: Slot,
    gen: number,
    scope: { before?: string },
  ): Promise<{ kind: "open"; opened: Opened } | { kind: "unpaired" | "none"; error?: MachineError }> {
    const { record } = slot;
    const first = await this.token(slot);
    if (gen !== slot.gen) return { kind: "none" };
    if (first === MINT_LATER) return { kind: "none", error: mintLaterError(record.name) };
    if (!first)
      return { kind: "unpaired", error: { code: "unpaired", message: errorMessage("unpaired", record.name) } };
    let token: string = first;
    let routes = this.candidates(slot);
    if (scope.before === undefined) slot.tried = new Set(routes.map(routeId));
    if (scope.before !== undefined) {
      const at = routes.findIndex((route) => routeId(route) === scope.before);
      routes = at < 0 ? [] : routes.slice(0, at);
    }
    let worst: MachineError | undefined;
    const note = (code: MachineErrorCode, route?: Route) => {
      if (!worst || ERROR_RANK[code] > ERROR_RANK[worst.code])
        worst = { code, message: errorMessage(code, record.name), ...(route ? { route } : {}) };
    };
    if (!routes.length) note("no_route");
    for (const route of routes) {
      if (gen !== slot.gen || this.disposed) return { kind: "none" };
      if (record.blockedRoutes?.includes(routeId(route))) {
        note("identity_mismatch", route);
        continue;
      }
      if (routeRefusal(route)) {
        note("insecure_route", route);
        continue;
      }
      const identity = await this.verifyRoute(slot, route);
      if (gen !== slot.gen) return { kind: "none" };
      if (!identity.ok) {
        note(identity.code, route);
        continue;
      }
      let retried = false;
      for (;;) {
        const outcome = await this.open(slot, route, token, identity.latencyMs);
        if (gen !== slot.gen) {
          if (outcome.kind === "open") outcome.opened.inner.close("Superseded");
          return { kind: "none" };
        }
        if (outcome.kind === "open") return outcome;
        if (outcome.kind === "revoked" && !retried) {
          // The machine no longer accepts this device's token. Another window may already have saved a fresh one
          // (minting revoked the token this one held): use that. Only the refused token is discarded, and then a
          // fresh one is minted through a member, once.
          retried = true;
          const { credentials } = this.options;
          const stored = await credentials.get(record.machineId);
          if (gen !== slot.gen) return { kind: "none" };
          if (stored && stored !== token) {
            token = stored;
            continue;
          }
          await (credentials.discard ? credentials.discard(record.machineId) : credentials.delete(record.machineId));
          const fresh = await this.token(slot);
          if (fresh === MINT_LATER) return { kind: "none", error: mintLaterError(record.name) };
          if (!fresh) return { kind: "unpaired", error: { code: "unpaired", message: errorMessage("unpaired", record.name) } };
          token = fresh;
          continue;
        }
        note("unreachable", route);
        break;
      }
    }
    return { kind: "none", ...(worst ? { error: worst } : {}) };
  }

  /**
   * The identity check of design §3.2. Nothing carrying a credential is sent to
   * an address before this passes. A machine that is not a member (a daemon
   * older than the network) has no identity document; it is left to the client's
   * own environment id check.
   */
  private async verifyRoute(
    slot: Slot,
    route: Route,
  ): Promise<{ ok: true; latencyMs?: number } | { ok: false; code: MachineErrorCode }> {
    const { record } = slot;
    if (!record.member) return { ok: true };
    if (this.options.pinPolicy === "require" && !record.signingKey) return { ok: false, code: "unpinned" };
    const nonce = this.transport.nonce();
    const started = this.clock.now();
    let answer: unknown;
    try {
      answer = await this.transport.probe(wellKnownUrl(route, nonce), { timeoutMs: probeTimeoutMs(route) });
    } catch {
      return { ok: false, code: "unreachable" };
    }
    const latencyMs = this.clock.now() - started;
    const check = await checkWellKnown(answer as WellKnownAnswer, {
      nonce,
      host: routeHost(route),
      now: this.clock.now(),
      expected: { machineId: record.machineId, ...(record.signingKey ? { signingKey: record.signingKey } : {}) },
      verifyProof: this.options.verifyProof,
    });
    if (check.ok) {
      if (!record.signingKey) {
        record.signingKey = check.signingKey;
        this.persist();
      }
      return { ok: true, latencyMs };
    }
    if (check.fatal) {
      // Never retried silently: block this address until the user clears it.
      const id = routeId(route);
      if (!record.blockedRoutes?.includes(id)) {
        record.blockedRoutes = [...(record.blockedRoutes ?? []), id].slice(-MAX_ROUTES_PER_MACHINE);
        this.persist();
      }
      return { ok: false, code: "identity_mismatch" };
    }
    return { ok: false, code: "clock" };
  }

  private async open(
    slot: Slot,
    route: Route,
    token: string,
    latencyMs: number | undefined,
  ): Promise<{ kind: "open"; opened: Opened } | { kind: "revoked" | "failed" }> {
    let inner: MachineClient;
    // A relay route is authenticated twice: the target's token end to end, and the member's own
    // token for the relay itself (design §3.4).
    let relayToken: string | undefined;
    if (route.kind === "relay") {
      relayToken = (route.via ? await this.options.credentials.get(route.via) : undefined) || undefined;
      if (!relayToken) return { kind: "failed" };
    }
    try {
      inner = this.transport.createClient({ record: slot.record, route, token, ...(relayToken ? { relayToken } : {}) });
    } catch {
      return { kind: "failed" };
    }
    const outcome = await new Promise<"open" | "revoked" | "failed">((resolve) => {
      let settled = false;
      let timer: unknown;
      const done = (value: "open" | "revoked" | "failed") => {
        if (settled) return;
        settled = true;
        this.clock.clearTimeout(timer);
        off();
        resolve(value);
      };
      const off = inner.onStatus((status) => {
        if (status === "open") done("open");
        else if (status === "failed" || status === "closed") done(inner.failure === "auth" ? "revoked" : "failed");
      });
      timer = this.clock.setTimeout(() => done("failed"), this.options.connectTimeoutMs ?? 15_000);
      inner.connect();
      if (inner.status === "open") done("open");
    });
    if (outcome !== "open") {
      inner.close("Route did not open");
      return { kind: outcome };
    }
    return { kind: "open", opened: { inner, route, ...(latencyMs !== undefined ? { latencyMs } : {}) } };
  }

  /** Make `opened` the machine's connection. The one it replaces is closed after subscriptions have moved. */
  private adopt(slot: Slot, opened: Opened) {
    const previous = slot.inner;
    const previousOwned = slot.owned;
    const wasLive = LIVE.has(slot.status);
    // A connection the app owns may not be open yet: it is listed offline until it opens.
    const live = opened.owned !== false || opened.inner.status === "open";
    slot.offInner?.();
    this.clock.clearTimeout(slot.graceTimer);
    this.cancelRetry(slot);
    slot.inner = opened.inner;
    slot.route = opened.route;
    slot.latencyMs = opened.latencyMs ?? slot.latencyMs;
    slot.error = undefined;
    slot.failures = 0;
    slot.lastSeenAt = this.clock.now();
    slot.owned = opened.owned !== false;
    slot.status = live ? this.liveStatus(slot) : "offline";
    slot.offInner = opened.inner.onStatus((status) => this.onInnerStatus(slot, opened.inner, status));
    slot.proxy.attach(opened.inner);
    if (previous && previous !== opened.inner && previousOwned) previous.close("Moved to another route");
    this.touch(slot, "status");
    // What the cache holds for this machine is replaced with a fresh read.
    if (!wasLive && live && slot.autoRefresh) this.refreshQuietly(slot.record.machineId);
    if (!wasLive && (slot.record.member || slot.inner.info?.features?.includes(NETWORK_FEATURES.network))) this.scheduleSync();
  }

  private liveStatus(slot: Slot): MachineConnectionStatus {
    if (slot.route?.kind === "relay") return "relaying";
    const features = slot.inner?.info?.features;
    return features?.includes(NETWORK_FEATURES.network) ? "online" : "needs-update";
  }

  private onInnerStatus(slot: Slot, inner: MachineClient, status: string) {
    if (slot.inner !== inner || this.disposed) return;
    if (status === "open") {
      this.clock.clearTimeout(slot.graceTimer);
      slot.lastSeenAt = this.clock.now();
      slot.error = undefined;
      slot.status = this.liveStatus(slot);
      this.touch(slot, "status");
      if (slot.autoRefresh) this.refreshQuietly(slot.record.machineId);
      if (slot.record.member || inner.info?.features?.includes(NETWORK_FEATURES.network)) this.scheduleSync();
    } else if (status === "reconnecting" || status === "connecting") {
      if (LIVE.has(slot.status)) {
        slot.status = "offline";
        slot.lastSeenAt = this.clock.now();
        this.touch(slot, "status");
      }
      this.clock.clearTimeout(slot.graceTimer);
      if (slot.external) return;
      // The client keeps trying its own route; after the grace period try the others.
      slot.graceTimer = this.clock.setTimeout(() => {
        if (slot.inner === inner && slot.status !== "paused") void this.failover(slot);
      }, this.options.reconnectGraceMs ?? 8_000);
    } else if (status === "failed" || status === "closed") {
      slot.lastSeenAt = this.clock.now();
      if (LIVE.has(slot.status)) {
        slot.status = "offline";
        this.touch(slot, "status");
      }
      if (!slot.external) void this.failover(slot);
    }
  }

  private async failover(slot: Slot): Promise<void> {
    if (slot.attempt) return;
    await this.connectSlot(slot);
  }

  /** The machine cannot be reached: keep its lists as the cache, close what was open, retry with backoff. */
  private goOffline(slot: Slot, status: "offline" | "unpaired" | "paused", error?: MachineError) {
    if (status === "paused") slot.gen++;
    this.clock.clearTimeout(slot.graceTimer);
    this.cancelRetry(slot);
    const inner = slot.inner;
    if (inner) slot.lastSeenAt ??= this.clock.now();
    slot.offInner?.();
    slot.offInner = undefined;
    slot.inner = undefined;
    slot.route = undefined;
    slot.proxy.attach(undefined);
    if (slot.owned) inner?.close("Machine unreachable");
    slot.status = slot.record.paused ? "paused" : status;
    slot.error = slot.record.paused ? undefined : error;
    this.touch(slot, "status");
    if (slot.record.paused || slot.external || this.disposed) return;
    let delay = FAILURE_DELAYS_MS[Math.min(slot.failures++, FAILURE_DELAYS_MS.length - 1)]!;
    // The daemon the user is at reaches it: keep trying more often than for a machine nobody sees.
    if (slot.daemonSaw === "online") delay = Math.min(delay, ONLINE_RETRY_MAX_MS);
    this.retryIn(slot, delay);
  }

  private cancelRetry(slot: Slot) {
    this.clock.clearTimeout(slot.retryTimer);
    slot.retryTimer = undefined;
    slot.retryAt = undefined;
  }

  private retryIn(slot: Slot, delay: number) {
    this.cancelRetry(slot);
    slot.retryAt = this.clock.now() + delay;
    slot.retryTimer = this.clock.setTimeout(() => {
      slot.retryAt = undefined;
      void this.connectSlot(slot);
    }, delay);
  }

  // ---- routes: better ones, learned ones ----

  private scheduleProbe() {
    this.clock.clearTimeout(this.probeTimer);
    if (this.disposed) return;
    this.probeTimer = this.clock.setTimeout(async () => {
      try {
        await Promise.all(
          [...this.slots.values()].map((slot) => (LIVE.has(slot.status) ? this.probeBetter(slot) : undefined)),
        );
      } finally {
        this.scheduleProbe();
      }
    }, this.options.probeIntervalMs ?? 300_000);
  }

  /** Probe the routes ranked ahead of the one in use and move to the first that verifies, keeping subscriptions. */
  private async probeBetter(slot: Slot): Promise<void> {
    if (slot.external || slot.attempt || slot.probing || !slot.route || !slot.inner || !LIVE.has(slot.status)) return;
    slot.probing = true;
    const gen = slot.gen;
    const current = slot.inner;
    try {
      const result = await this.findRoute(slot, gen, { before: routeId(slot.route) });
      if (result.kind !== "open") return;
      if (gen !== slot.gen || slot.inner !== current) {
        result.opened.inner.close("Superseded");
        return;
      }
      this.adopt(slot, result.opened);
    } catch {
      // The route in use keeps working.
    } finally {
      slot.probing = false;
    }
  }

  private learn(slot: Slot, routes: readonly Route[] | undefined) {
    if (!routes) return;
    const usable = routes.filter((route) => !!route && typeof route.url === "string" && (route.kind !== "relay" || !!route.via));
    const kept = orderRoutes(usable).slice(0, MAX_ROUTES_PER_MACHINE);
    const same = JSON.stringify(slot.record.learned ?? []) === JSON.stringify(kept);
    slot.record.learned = kept;
    if (!same) {
      this.persist();
      this.touch(slot, "routes");
    }
  }

  // ---- the network: members, tokens ----

  /**
   * A live saved computer that speaks the network protocol though it is not (yet) known as a member: what the set asks
   * when the machine the user is at is down, so one dead computer does not hide the whole network.
   */
  private networkSpeaker(): Slot | undefined {
    return [...this.slots.values()]
      // A connection the app owns is the machine the user is at (asked first, above), never a fallback.
      .filter((slot) => slot.inner && !slot.external && !slot.record.paused && LIVE.has(slot.status) && slot.inner.info?.features?.includes(NETWORK_FEATURES.network))
      .sort((a, b) => (a.latencyMs ?? Infinity) - (b.latencyMs ?? Infinity))[0];
  }

  /** The connected member to ask: the local machine, else the nearest. */
  private bestMember(except?: Slot): Slot | undefined {
    return [...this.slots.values()]
      .filter(
        (slot) =>
          slot !== except &&
          slot.inner &&
          slot.record.member &&
          (slot.status === "online" || slot.status === "relaying") &&
          slot.inner.info?.features?.includes(NETWORK_FEATURES.network) !== false,
      )
      .sort((a, b) => {
        const local = Number(b.record.machineId === this.localId) - Number(a.record.machineId === this.localId);
        return local || (a.latencyMs ?? Infinity) - (b.latencyMs ?? Infinity);
      })[0];
  }

  /**
   * This device's token for a machine: the saved one, else one minted through a member (§3.4), at most as often as
   * `MINT_MIN_INTERVAL_MS` and its backoff allow (`MINT_LATER` otherwise).
   */
  private async token(slot: Slot): Promise<string | undefined | typeof MINT_LATER> {
    const saved = await this.options.credentials.get(slot.record.machineId);
    if (saved) return saved;
    const member = this.bestMember(slot);
    if (!member?.inner) return undefined;
    const now = this.clock.now();
    const wait = slot.mintWaitMs ?? MINT_MIN_INTERVAL_MS;
    if (slot.mintedAt !== undefined && now - slot.mintedAt < wait) return MINT_LATER;
    // Mints that follow each other within twice the wait are churn: wait longer next time. Otherwise start over.
    slot.mintWaitMs =
      slot.mintedAt !== undefined && now - slot.mintedAt < wait * 2 ? Math.min(wait * 2, MINT_MAX_INTERVAL_MS) : MINT_MIN_INTERVAL_MS;
    slot.mintedAt = now;
    try {
      const grant = (await member.inner.call("network.client_grant", {
        machine_id: slot.record.machineId,
        device_id: this.options.deviceId,
      })) as NetworkClientGrantResult;
      if (!grant?.token) return undefined;
      await this.options.credentials.set(slot.record.machineId, grant.token);
      if (grant.signing_key && !slot.record.signingKey) slot.record.signingKey = grant.signing_key;
      this.learn(slot, grant.routes);
      this.persist();
      return grant.token;
    } catch {
      return undefined;
    }
  }

  private scheduleSync(from?: Slot, params?: unknown) {
    if (this.disposed) return;
    // `network.changed` carries the whole status: the sync uses it instead of asking again when it asks that member.
    if (from && isNetworkStatus(params)) this.pushed = { slot: from, status: params };
    if (this.syncing) { this.syncAgain = true; return; }
    this.clock.clearTimeout(this.syncTimer);
    this.syncTimer = this.clock.setTimeout(() => void this.syncMembers(), 300);
  }

  /**
   * Ask a member for the network and make the machine list match: new members
   * are added, ones removed anywhere are forgotten here, names, pauses and
   * routes follow. Machines that are not members (older daemons saved by the
   * user) are never touched.
   */
  async syncMembers(): Promise<void> {
    if (this.syncing) { this.syncAgain = true; return this.syncing; }
    const run = (async () => {
      do {
        this.syncAgain = false;
        await this.syncMembersOnce();
      } while (this.syncAgain && !this.disposed);
    })();
    this.syncing = run.finally(() => {
      this.syncing = undefined;
    });
    return this.syncing;
  }

  private async syncMembersOnce(): Promise<void> {
    // The authenticated primary may have enabled networking since it opened. Ask it even when
    // every saved record says member:false; only its answer can promote those records.
    const local = this.localId ? this.slots.get(this.localId) : undefined;
    const source = local?.inner && LIVE.has(local.status) && local.inner.info?.features?.includes(NETWORK_FEATURES.network)
      ? local : this.bestMember() ?? this.networkSpeaker();
    if (!source?.inner) return;
    // Only the machine the user is at, or one already known to be in the network, may forget machines. A saved
    // computer asked because the local one is down may belong to another network: it adds and updates, never removes.
    const authoritative = source === local || source.record.member;
    const inner = source.inner;
    const gen = source.gen;
    const localId = this.localId;
    const current = () => !this.disposed && this.localId === localId && this.slots.get(source.record.machineId) === source && source.inner === inner && source.gen === gen && LIVE.has(source.status);
    let members: MachineCard[];
    const pushed = this.pushed;
    this.pushed = undefined;
    try {
      const status = pushed?.slot === source ? pushed.status : await inner.call("network.status", {});
      if (!current() || this.syncAgain || status.self_machine.machine_id !== source.record.machineId) return;
      // Turning the switch off keeps the saved network, routes and credentials. It must not
      // become an authoritative empty member list (or mint grants through this connection).
      if (!status.enabled || status.removed || !status.members.some((card) => card.machine_id === source.record.machineId)) {
        if (source.record.member) {
          source.record.member = false;
          this.persist();
          this.touch(source, "records");
        }
        return;
      }
      members = status.members;
    } catch {
      return;
    }
    const ids = new Set(members.map((card) => card.machine_id));
    const fresh: Slot[] = [];
    for (const card of members) {
      let slot = this.slots.get(card.machine_id);
      // A saved profile can still carry a stand-in id while its authenticated socket has
      // already answered with the real id. Move it before creating a duplicate network row.
      if (!slot) {
        const saved = [...this.slots.values()].find((other) => !other.record.member &&
          (other.inner?.info?.machine_id || other.inner?.info?.environment_id) === card.machine_id);
        if (saved) {
          try { await this.rekeyMachine(saved.record.machineId, card.machine_id); } catch { /* the hub may have re-keyed it first */ }
          if (!current() || this.syncAgain) return;
          slot = this.slots.get(card.machine_id);
        }
      }
      if (!slot) {
        if (this.slots.size >= (this.options.maxMachines ?? 32)) continue;
        slot = this.createSlot({
          machineId: card.machine_id,
          name: card.name,
          os: card.os,
          routes: [],
          member: true,
          paused: card.status === "paused",
          ...(card.signing_key ? { signingKey: card.signing_key } : {}),
          ...(card.ts_name ? { tsName: card.ts_name } : {}),
        });
        fresh.push(slot);
      } else {
        const record = slot.record;
        const paused = card.status === "paused";
        const tsName = card.ts_name ?? undefined;
        const changed = record.name !== card.name || record.os !== card.os || !record.member || !!record.paused !== paused || record.tsName !== tsName;
        record.name = card.name;
        if (tsName) record.tsName = tsName;
        else delete record.tsName;
        record.os = card.os;
        record.member = true;
        if (card.signing_key && !record.signingKey) record.signingKey = card.signing_key;
        if (!!record.paused !== paused) {
          record.paused = paused;
          if (paused) this.goOffline(slot, "paused");
          else fresh.push(slot);
        }
        if (changed) this.touch(slot, "records");
      }
    }
    for (const slot of authoritative ? [...this.slots.values()] : [])
      if (slot.record.member && !ids.has(slot.record.machineId)) {
        await this.removeMachine(slot.record.machineId);
        if (!current() || this.syncAgain) return;
      }
    for (const slot of this.slots.values()) {
      if (!slot.record.member || slot === source) continue;
      try {
        const result = await inner.call("network.routes", { machine_id: slot.record.machineId });
        if (!current() || this.syncAgain) return;
        this.learn(slot, result.routes);
      } catch {
        // Routes are refreshed on the next sync.
      }
    }
    // The machine the user is at saw these members come back (offline at the last sync, online now): one that is
    // offline here (waiting out its backoff, or with routes learned while it was away) is dialed now, so it comes back
    // without an app restart. A member the daemon has reported online all along and this device cannot reach (a phone
    // off the tailnet) keeps its backoff; only an attempt of its that is stuck is replaced (`connectSlot`). Not
    // awaited: a dial that hangs must not hold up the next sync.
    const redial: Slot[] = [];
    for (const card of members) {
      const slot = this.slots.get(card.machine_id);
      if (!slot) continue;
      const cameBack = card.status === "online" && slot.daemonSaw !== undefined && slot.daemonSaw !== "online";
      slot.daemonSaw = card.status;
      // Reported online while this device has it offline and waiting longer than the online cap (it failed before the
      // daemon's first answer, at launch say): its next attempt moves up to the cap. Never sooner, so a member this
      // device cannot reach is not searched every sync.
      if (card.status === "online" && slot !== source && wantsDial(slot) && !slot.attempt && slot.retryAt !== undefined &&
        slot.retryAt - this.clock.now() > ONLINE_RETRY_MAX_MS) this.retryIn(slot, ONLINE_RETRY_MAX_MS);
      const stuck = card.status === "online" && slot.attempt !== undefined && this.clock.now() - (slot.attemptAt ?? 0) >= ATTEMPT_STALE_MS;
      if (!(cameBack || stuck) || slot === source || !wantsDial(slot) || fresh.includes(slot)) continue;
      // At most every REDIAL_MS, however often it flaps.
      if (cameBack && slot.redialAt !== undefined && this.clock.now() - slot.redialAt < REDIAL_MS) continue;
      if (cameBack) {
        slot.redialAt = this.clock.now();
        slot.failures = 0;
        this.cancelRetry(slot);
      }
      redial.push(slot);
    }
    this.persist();
    this.touch(undefined, "records");
    for (const slot of redial) void this.connectSlot(slot);
    await mapLimit(fresh, 4, (slot) => this.connectSlot(slot));
  }
}

/** A machine the set dials itself that is not connected. One with an attempt under way joins it (`connectSlot`). */
function wantsDial(slot: Slot): boolean {
  return !slot.external && !slot.record.paused && !LIVE.has(slot.status) && slot.status !== "unpaired";
}

/** A `network.changed` payload that is a whole `NetworkStatus` (older daemons send `{}`). */
export function isNetworkStatus(value: unknown): value is NetworkStatus {
  const status = value as Partial<NetworkStatus> | null | undefined;
  return !!status && typeof status === "object" && typeof status.enabled === "boolean" && Array.isArray(status.members) &&
    typeof status.self_machine?.machine_id === "string";
}
