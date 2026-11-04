// One transport implementation for desktop and mobile. Socket opening,
// authentication, identity verification, and state synchronization are distinct.
import {
  PROTOCOL_VERSION,
  EVENT_NOTIFICATION,
  EVENTS_LAGGED_NOTIFICATION,
  EVENTS_READY_NOTIFICATION,
  type EventsReadyNotification,
  codes,
  type DaemonInfo,
  type EventNotification,
  type EventsSubscribeParams,
  type MethodName,
  type ParamsOf,
  type ResultOf,
  type RpcError,
  type RpcNotification,
  type RpcResponse,
  type SubscriptionId,
  type ThreadEvent,
  type Problem,
} from "./types.ts";
import { httpBase } from "./address.ts";
import { problemOf } from "./problem.ts";

export interface Endpoint {
  url: string;
  token: string;
  environmentId?: string;
}
export type ConnectionStatus =
  "idle" | "connecting" | "open" | "reconnecting" | "failed" | "closed";
export class RpcCallError extends Error {
  readonly code: number;
  readonly data: unknown;
  /** What went wrong in terms a person can act on (`data.problem`), when the daemon sent one. */
  readonly problem?: Problem;
  constructor(error: RpcError) {
    super(error.message);
    this.name = "RpcCallError";
    this.code = error.code;
    this.data = error.data;
    const problem = problemOf(error);
    if (problem) this.problem = problem;
  }
  /** Only the message, so a stray `String(error)` never shows "RpcCallError:" to a person. */
  toString(): string {
    return this.message;
  }
}
export class ConnectionClosedError extends Error {
  constructor(message = "Connection closed") {
    super(message);
    this.name = "ConnectionClosedError";
  }
}

/** Why a client stopped for good: the machine refused its credential. */
export type ClientFailure = "auth";

export interface ClientOptions {
  expectedEnvironmentId?: string;
  /** Native platforms can authenticate the upgrade with a header. Browsers
   * exchange the credential for a short-lived, single-use socket ticket. */
  createSocket?: (url: string, token: string) => WebSocket;
  /**
   * A native socket that is refused at the upgrade reports only "closed before open": the HTTP
   * status (401) never reaches the page. After such a close this asks the machine directly whether
   * the credential is what it refused. `"rejected"` fails the client with `failure === "auth"`;
   * anything else keeps retrying.
   */
  probeAuth?: () => Promise<"rejected" | "accepted" | "unknown">;
  requestTimeoutMs?: number;
  heartbeatMs?: number;
  /**
   * Where `fetch` sends HTTP requests, when it is not the socket address's own base: a relay route
   * reaches the target under the member's `/relay/<id>/http`, and needs the member's own `headers`.
   */
  http?: { base: string; headers?: Record<string, string> };
  /**
   * Serves one of the machine's ticket routes on a loopback origin of its own (`openPage`). Set by an
   * app for a connection to another machine: a web view or the system browser cannot send the relay
   * member's header, so the opener adds whatever the route needs and the page never sees it.
   */
  pageRelay?: PageRelay;
}

/** An authenticated request a native loader makes itself (`KybernClient.request`). */
export interface DaemonRequest {
  url: string;
  headers: Record<string, string>;
}

/** A page of the machine a web view or browser can load: `close` ends its relay, if it has one. */
export interface DaemonPage {
  url: string;
  close(): void;
}
export interface PageOptions {
  /** A preview's WebSocket (hot reload) was refused on the way. */
  onWebSocketFailure?: () => void;
}
/** Opens a loopback relay to `upstream`, a ticket route on the client's own route. */
export type PageRelay = (upstream: string, options: PageOptions) => Promise<DaemonPage>;

type Pending = {
  resolve: (value: unknown) => void;
  reject: (error: Error) => void;
  timer: ReturnType<typeof setTimeout>;
};
type StatusHandler = (status: ConnectionStatus, detail?: string) => void;
export type EventHandler = (event: ThreadEvent) => void;
export interface ReplayState {
  /** A previous delivery cursor was used, rather than a new live-only subscription. */
  resumed: boolean;
  /** The server will signal replay completion with events.ready. */
  supported: boolean;
}
interface EventSubscription {
  params: EventsSubscribeParams;
  handler: EventHandler;
  lastSeq: number | undefined;
  remoteId: SubscriptionId | undefined;
  generation: number;
  onSubscribed?: (headSeq: number, replay: ReplayState) => void;
  onReady?: (headSeq: number, resumed: boolean) => void;
  ready: { headSeq: number; resumed: boolean } | undefined;
}
export interface Subscription {
  unsubscribe(): void;
  readonly lastSeq: number | undefined;
}

export class KybernClient {
  readonly endpoint: Endpoint;
  private readonly options: ClientOptions;
  private ws: WebSocket | null = null;
  private nextId = 1;
  private pending = new Map<number, Pending>();
  private notifications = new Map<string, Set<(params: unknown) => void>>();
  private statuses = new Set<StatusHandler>();
  private subscriptions = new Set<EventSubscription>();
  private byRemoteId = new Map<SubscriptionId, EventSubscription>();
  private _status: ConnectionStatus = "idle";
  private _failure: ClientFailure | undefined;
  private _info: DaemonInfo | null = null;
  private stopped = true;
  private attempt = 0;
  private generation = 0;
  private reconnectTimer: ReturnType<typeof setTimeout> | undefined;
  private heartbeat: ReturnType<typeof setInterval> | undefined;
  private openTimer: ReturnType<typeof setTimeout> | undefined;
  private opening: AbortController | undefined;
  // Discovery starts provider processes. Share concurrent requests and reuse
  // recent results across pickers and sends, scoped to this verified endpoint.
  private skills = new Map<string, { expires: number; value: Promise<unknown> }>();

  constructor(endpoint: Endpoint, options: ClientOptions = {}) {
    this.endpoint = endpoint;
    this.options = options;
  }
  get status() {
    return this._status;
  }
  get info() {
    return this._info;
  }
  /** Set when the client failed because the credential was refused (revoked or never valid). */
  get failure() {
    return this._failure;
  }

  connect() {
    if (!this.stopped) return;
    this.stopped = false;
    this.attempt = 0;
    this._failure = undefined;
    void this.open();
  }

  close(reason = "Connection closed") {
    this.stopped = true;
    this.generation++;
    clearTimeout(this.reconnectTimer);
    this.dropSocket(new ConnectionClosedError(reason));
    this.skills.clear();
    this.subscriptions.clear();
    this.byRemoteId.clear();
    this.setStatus("closed", reason);
  }

  onStatus(handler: StatusHandler) {
    this.statuses.add(handler);
    return () => {
      this.statuses.delete(handler);
    };
  }
  onNotification(method: string, handler: (params: unknown) => void) {
    let handlers = this.notifications.get(method);
    if (!handlers) {
      handlers = new Set();
      this.notifications.set(method, handlers);
    }
    handlers.add(handler);
    return () => {
      handlers.delete(handler);
    };
  }

  call<M extends MethodName>(
    method: M,
    params: ParamsOf<M>,
  ): Promise<ResultOf<M>> {
    if (method === "skills.list" && this._status === "open") {
      const input = params as ParamsOf<"skills.list">;
      const key = JSON.stringify([input.project_id, input.provider]);
      const cached = this.skills.get(key);
      if (cached && cached.expires > Date.now())
        return cached.value as Promise<ResultOf<M>>;
      const entry = { expires: Infinity, value: this.callRaw(method, params) };
      this.skills.delete(key);
      this.skills.set(key, entry);
      while (this.skills.size > 24) this.skills.delete(this.skills.keys().next().value!);
      void entry.value.then(() => { entry.expires = Date.now() + 60_000; }, () => {
        if (this.skills.get(key) === entry) this.skills.delete(key);
      });
      return entry.value as Promise<ResultOf<M>>;
    }
    // Configuration/installation changes must be visible on the next open.
    if (method === "settings.update" || method === "integrations.change") this.skills.clear();
    return this.callRaw(method, params) as Promise<ResultOf<M>>;
  }

  /**
   * An authenticated HTTP request to one of this machine's routes (`/assets`, `/threads/…`), on the
   * route this client is connected through. `path` is relative to the daemon; the address and the
   * token stay in the client.
   */
  fetch(path: string, init: RequestInit = {}): Promise<Response> {
    let target: DaemonRequest;
    try {
      target = this.request(path);
    } catch (error) {
      return Promise.reject(error);
    }
    const headers = new Headers(init.headers);
    for (const [name, value] of Object.entries(target.headers)) headers.set(name, value);
    try {
      return fetch(target.url, { ...init, headers, redirect: "error" });
    } catch (error) {
      return Promise.reject(error);
    }
  }

  /**
   * The URL and headers of an authenticated request to one of this machine's routes, for a loader
   * that makes the request itself (an `Image` source, a file download, a native upload). On a relay
   * route the URL goes through the member and the headers carry its token beside this machine's.
   * Hand both to the loader as they are; never put them in a page or a log.
   */
  request(path: string): DaemonRequest {
    if (!path.startsWith("/") || path.startsWith("//")) throw new Error("A daemon request needs a path that starts with /");
    const http = this.options.http;
    return {
      url: `${http?.base ?? httpBase(this.endpoint.url)}${path}`,
      headers: { ...http?.headers, authorization: `Bearer ${this.endpoint.token}` },
    };
  }

  /**
   * A ticket route of this machine (`/visual-frame/<ticket>`, `/artifact-preview/<ticket>`,
   * `/preview-proxy/<ticket>/`) as an address a web view or browser can load. Without a `pageRelay`
   * it is the daemon's own address, as for the machine the app runs beside; with one, a loopback
   * relay to the client's route. Tickets carry no credential, and the address never does.
   */
  openPage(path: string, options: PageOptions = {}): Promise<DaemonPage> {
    if (!path.startsWith("/") || path.startsWith("//"))
      return Promise.reject(new Error("A daemon page needs a path that starts with /"));
    try {
      const upstream = `${this.options.http?.base ?? httpBase(this.endpoint.url)}${path}`;
      if (this.options.pageRelay) return this.options.pageRelay(upstream, options);
      return Promise.resolve({ url: upstream, close() {} });
    } catch (error) {
      return Promise.reject(error);
    }
  }

  callRaw(
    method: string,
    params: unknown,
    timeoutMs = this.options.requestTimeoutMs ?? 120_000,
  ): Promise<unknown> {
    const socket = this.ws;
    if (
      !socket ||
      socket.readyState !== 1 ||
      (this._status !== "open" && method !== "daemon.info")
    ) {
      return Promise.reject(
        new ConnectionClosedError(
          "Reconnect to this environment before trying again",
        ),
      );
    }
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(
          new ConnectionClosedError(
            "The environment did not acknowledge this request. Reconnect and check its state before retrying",
          ),
        );
      }, timeoutMs);
      this.pending.set(id, { resolve, reject, timer });
      try {
        socket.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
      } catch (e) {
        clearTimeout(timer);
        this.pending.delete(id);
        reject(e instanceof Error ? e : new Error(String(e)));
      }
    });
  }

  subscribeEvents(
    params: EventsSubscribeParams,
    handler: EventHandler,
    onSubscribed?: (headSeq: number, replay: ReplayState) => void,
    onReady?: (headSeq: number, resumed: boolean) => void,
  ): Subscription {
    const sub: EventSubscription = {
      params,
      handler,
      onSubscribed,
      onReady,
      ready: undefined,
      lastSeq: params.after_seq,
      remoteId: undefined,
      generation: 0,
    };
    this.subscriptions.add(sub);
    if (this._status === "open") void this.issueSubscribe(sub);
    return {
      unsubscribe: () => {
        this.subscriptions.delete(sub);
        sub.generation++;
        if (sub.remoteId) {
          this.byRemoteId.delete(sub.remoteId);
          void this.call("events.unsubscribe", {
            subscription_id: sub.remoteId,
          }).catch(() => {});
          sub.remoteId = undefined;
        }
      },
      get lastSeq() {
        return sub.lastSeq;
      },
    };
  }

  /** Also used when the app returns from background or the network changes. */
  async checkConnection(): Promise<void> {
    if (this.stopped) return;
    if (this._status !== "open") {
      if (!this.ws && !this.opening) {
        clearTimeout(this.reconnectTimer);
        void this.open();
      }
      return;
    }
    const socket = this.ws;
    try {
      await this.callRaw("daemon.info", {}, 10_000);
    } catch {
      if (this.ws === socket && !this.stopped)
        this.retry("Connection interrupted");
    }
  }

  private async open() {
    if (this.stopped) return;
    const generation = ++this.generation;
    this.setStatus(this.attempt ? "reconnecting" : "connecting");
    const controller = new AbortController();
    this.opening?.abort();
    this.opening = controller;
    this.openTimer = setTimeout(() => {
      if (this.generation === generation)
        this.retry("Connection timed out; check the environment and network");
    }, 15_000);
    try {
      let socket: WebSocket;
      if (this.options.createSocket) {
        socket = this.options.createSocket(
          this.endpoint.url,
          this.endpoint.token,
        );
      } else {
        const response = await fetch(`${httpBase(this.endpoint.url)}/session`, {
          method: "POST",
          headers: { authorization: `Bearer ${this.endpoint.token}` },
          signal: controller.signal,
          redirect: "error",
        });
        if (this.stopped || generation !== this.generation) return;
        if (response.status === 401 || response.status === 403) {
          this.fail(
            "Access was revoked or the credential is invalid. Pair this environment again",
            "auth",
          );
          return;
        }
        if (response.status === 404) {
          this.fail(
            "Update Kybern on this machine to enable remote connections",
          );
          return;
        }
        if (!response.ok)
          throw new Error(`Connection setup failed (${response.status})`);
        const { ticket } = (await response.json()) as { ticket: string };
        if (typeof ticket !== "string" || !ticket)
          throw new Error(
            "The environment returned an invalid connection ticket",
          );
        if (this.stopped || generation !== this.generation) return;
        const url = new URL(this.endpoint.url);
        url.searchParams.set("ticket", ticket);
        socket = new WebSocket(url.toString());
      }
      if (this.stopped || generation !== this.generation) {
        socket.close();
        return;
      }
      this.ws = socket;
      socket.onmessage = (event) => {
        if (this.ws === socket) this.frame(String(event.data));
      };
      socket.onerror = () => {}; // onclose owns retries; never expose a credential-bearing URL.
      let opened = false;
      socket.onclose = (event) => {
        if (this.ws !== socket || this.stopped) return;
        if (event.code === 4401)
          this.fail("Access was revoked. Pair this environment again", "auth");
        else if (!opened && this.options.createSocket && this.options.probeAuth)
          void this.probeRefusal(socket, generation);
        else
          this.retry(
            "Connection interrupted; reconnecting to this environment",
          );
      };
      socket.onopen = async () => {
        opened = true;
        try {
          const info = (await this.callRaw(
            "daemon.info",
            {},
            10_000,
          )) as DaemonInfo;
          if (this.ws !== socket) return;
          if (info.protocol_version !== PROTOCOL_VERSION) {
            this.fail(
              "This environment uses an incompatible protocol. Update Kybern on the client and host",
            );
            return;
          }
          const expected =
            this.options.expectedEnvironmentId ??
            this.endpoint.environmentId ??
            this._info?.environment_id;
          if (
            !info.environment_id ||
            (expected && info.environment_id !== expected)
          ) {
            this.fail(
              "This address now belongs to a different environment. Add it separately",
            );
            return;
          }
          this.opening = undefined;
          this._info = info;
          clearTimeout(this.openTimer);
          this.attempt = 0;
          this.setStatus("open");
          for (const sub of this.subscriptions) void this.issueSubscribe(sub);
          this.heartbeat = setInterval(() => {
            void this.checkConnection();
          }, this.options.heartbeatMs ?? 20_000);
        } catch {
          if (this.ws === socket && !this.stopped)
            this.retry("Unable to verify the environment; reconnecting");
        }
      };
    } catch (e) {
      if (!this.stopped && generation === this.generation)
        this.retry(
          e instanceof Error && e.name !== "AbortError"
            ? e.message
            : "Connection interrupted",
        );
    }
  }

  private dropSocket(error: Error) {
    this.opening?.abort();
    this.opening = undefined;
    clearTimeout(this.openTimer);
    clearInterval(this.heartbeat);
    const socket = this.ws;
    this.ws = null;
    if (socket) {
      socket.onopen = socket.onmessage = socket.onerror = socket.onclose = null;
      socket.close();
    }
    for (const pending of this.pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(error);
    }
    this.pending.clear();
    this.byRemoteId.clear();
    for (const sub of this.subscriptions) {
      sub.remoteId = undefined;
      sub.ready = undefined;
      sub.generation++;
    }
  }

  private retry(detail: string) {
    if (this.stopped) return;
    this.generation++;
    this.dropSocket(new ConnectionClosedError(detail));
    clearTimeout(this.reconnectTimer);
    const delay =
      Math.min(15_000, 500 * 2 ** this.attempt++) * (0.5 + Math.random() / 2);
    this.setStatus("reconnecting", detail);
    this.reconnectTimer = setTimeout(() => {
      void this.open();
    }, delay);
  }

  /** The upgrade was refused without a status: find out whether the credential is why. */
  private async probeRefusal(socket: WebSocket, generation: number) {
    let verdict: "rejected" | "accepted" | "unknown" = "unknown";
    try {
      verdict = await this.options.probeAuth!();
    } catch {
      // unreachable or timed out: an ordinary retry
    }
    if (this.stopped || this.ws !== socket || generation !== this.generation) return;
    if (verdict === "rejected")
      this.fail(
        "Access was revoked or the credential is invalid. Pair this environment again",
        "auth",
      );
    else this.retry("Connection interrupted; reconnecting to this environment");
  }

  private fail(detail: string, failure?: ClientFailure) {
    this._failure = failure;
    this.close(detail);
    this.setStatus("failed", detail);
  }

  private setStatus(status: ConnectionStatus, detail?: string) {
    this._status = status;
    for (const handler of this.statuses) handler(status, detail);
  }

  private frame(text: string) {
    let frame: RpcResponse | RpcNotification;
    try {
      frame = JSON.parse(text);
    } catch {
      return;
    }
    if ("id" in frame && typeof frame.id === "number") {
      const pending = this.pending.get(frame.id);
      if (!pending) return;
      this.pending.delete(frame.id);
      clearTimeout(pending.timer);
      if (frame.error) pending.reject(new RpcCallError(frame.error));
      else pending.resolve(frame.result);
    } else if ("method" in frame) {
      if (frame.method === EVENT_NOTIFICATION) {
        const params = frame.params as EventNotification;
        const sub =
          params?.event && this.byRemoteId.get(params.subscription_id);
        if (
          sub &&
          (sub.lastSeq === undefined || params.event.seq > sub.lastSeq)
        ) {
          sub.lastSeq = params.event.seq;
          sub.handler(params.event);
        }
      } else if (frame.method === EVENTS_READY_NOTIFICATION) {
        const ready = frame.params as EventsReadyNotification;
        const sub = ready && this.byRemoteId.get(ready.subscription_id);
        if (sub?.ready && ready.head_seq === sub.ready.headSeq) {
          const { resumed } = sub.ready;
          sub.ready = undefined;
          // For a filtered subscription there may be no event at the global
          // head. The barrier still advances its safe reconnect cursor.
          sub.lastSeq = Math.max(sub.lastSeq ?? 0, ready.head_seq);
          sub.onReady?.(ready.head_seq, resumed);
        }
      } else if (frame.method === EVENTS_LAGGED_NOTIFICATION) {
        for (const sub of this.subscriptions) void this.issueSubscribe(sub);
      }
      for (const handler of this.notifications.get(frame.method) ?? [])
        handler(frame.params);
    }
  }

  private async issueSubscribe(sub: EventSubscription) {
    const generation = ++sub.generation;
    const socket = this.ws;
    const resumed = sub.lastSeq !== undefined;
    sub.ready = undefined;
    if (sub.remoteId) {
      this.byRemoteId.delete(sub.remoteId);
      void this.call("events.unsubscribe", {
        subscription_id: sub.remoteId,
      }).catch(() => {});
      sub.remoteId = undefined;
    }
    try {
      const result = await this.call("events.subscribe", {
        ...sub.params,
        after_seq: sub.lastSeq,
      });
      if (
        !this.subscriptions.has(sub) ||
        sub.generation !== generation ||
        this.ws !== socket
      ) {
        if (this.ws === socket)
          void this.call("events.unsubscribe", {
            subscription_id: result.subscription_id,
          }).catch(() => {});
        return;
      }
      sub.remoteId = result.subscription_id;
      this.byRemoteId.set(result.subscription_id, sub);
      // A live-only subscription starts at its acknowledged head even if no
      // new events arrive before the first disconnect.
      sub.lastSeq ??= result.head_seq;
      if (result.replay_ready) sub.ready = { headSeq: result.head_seq, resumed };
      sub.onSubscribed?.(result.head_seq, { resumed, supported: result.replay_ready === true });
    } catch (error) {
      if (
        this.ws !== socket ||
        this._status !== "open" ||
        !this.subscriptions.has(sub) ||
        sub.generation !== generation
      )
        return;
      if (
        error instanceof RpcCallError &&
        error.code === codes.INVALID_PARAMS
      ) {
        if (sub.lastSeq !== undefined) {
          // A restored database can move the event head backwards. Start a
          // live-only subscription and let callers replace their stale snapshots.
          sub.lastSeq = undefined;
          void this.issueSubscribe(sub);
        } else {
          this.fail("Unable to subscribe to this environment. Update Kybern and reconnect");
        }
      } else {
        this.retry("Unable to synchronize this environment");
      }
    }
  }
}
