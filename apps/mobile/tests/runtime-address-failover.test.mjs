// The phone's own computer stops answering at its saved address while the computer is fine: an old daemon that never
// finished shutting down still holds that port (it answers the identity check, then closes every session) and the live
// daemon listens on another port. Another member of the network knows the new address. The phone must move to it, the
// Machines page must keep rendering while the computer reads offline, and Home must keep working through the network.
import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { registerHooks } from "node:module";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { ed25519 } from "@noble/curves/ed25519.js";
import { nativeModuleSource, seedSecure } from "./native-mocks.mjs";
import { PROTOCOL_VERSION } from "../../../packages/kybern-client/src/types.ts";

const STALE = "http://mac.tail0745fc.ts.net:59930";
const FRESH = "http://mac.tail0745fc.ts.net:4175";
const VPS = "http://vps.tail0745fc.ts.net:4173";
const records = [
  { machineId: "mac", name: "Treshnandas-Macbook", routes: [{ kind: "direct_advertised", url: STALE }], member: true },
  { machineId: "vps", name: "kdi-vps", routes: [{ kind: "direct_tailnet", url: VPS }], member: true },
];
seedSecure({
  "kybern.ink.machines": JSON.stringify({ version: 1, records }),
  "kybern.ink.active": "mac",
  "kybern.ink.token.mac": "tok-mac",
  "kybern.ink.token.vps": "tok-vps",
});
const unsubscribers = [];
globalThis.reconnectSubscribe = (subscribe, getSnapshot) => {
  unsubscribers.push(subscribe(() => {}));
  return getSnapshot();
};
registerHooks({ resolve(specifier, context, next) {
  const source = nativeModuleSource(specifier);
  if (source) return { shortCircuit: true, url: `data:text/javascript,${encodeURIComponent(source)}` };
  const url = specifier.startsWith(".") && context.parentURL ? new URL(specifier, context.parentURL) : null;
  if (url?.protocol === "file:" && !/\.[a-z]+$/i.test(url.pathname) && existsSync(fileURLToPath(url) + ".ts")) return { shortCircuit: true, url: url.href + ".ts" };
  return next(specifier, context);
} });

// Real Ed25519 identities: the stale port and the fresh one are the same machine (the same data directory).
const b64 = (bytes) => Buffer.from(bytes).toString("base64");
const secrets = { mac: ed25519.utils.randomSecretKey(), vps: ed25519.utils.randomSecretKey() };
const keys = Object.fromEntries(Object.entries(secrets).map(([id, secret]) => [id, b64(ed25519.getPublicKey(secret))]));
const machineAt = (url) => (url.includes("vps.") ? "vps" : url.includes("mac.") ? "mac" : null);
const { wellKnownProofMessage } = await import("../../../packages/kybern-client/src/machineRoutes.ts");
const previousFetch = globalThis.fetch;
globalThis.fetch = async (input) => {
  const url = new URL(String(input));
  const machineId = machineAt(url.href);
  if (!url.pathname.endsWith("/.well-known/kybern/machine") || !machineId) throw new TypeError("Network request failed");
  const at = new Date(Date.now()).toISOString();
  const proof = ed25519.sign(wellKnownProofMessage({ nonce: url.searchParams.get("nonce"), machineId, at, host: url.host }), secrets[machineId]);
  const body = { machine_id: machineId, name: machineId, os: "macos", signing_key: keys[machineId], at, proof: b64(proof), network_protocol: 1, features: ["network.v1"] };
  return { ok: true, status: 200, json: async () => body };
};

const card = (id) => ({ machine_id: id, name: id === "mac" ? "Treshnandas-Macbook" : "kdi-vps", os: "linux", status: "online", signing_key: keys[id] });
const sockets = [];
class Socket {
  readyState = 0; sent = [];
  constructor(url, _protocols, options) {
    this.url = url; this.headers = options?.headers; sockets.push(this);
    const stale = url.includes(":59930");
    // The old daemon: the socket closes before it opens. Everything else opens.
    queueMicrotask(() => (stale ? this.onclose?.({ code: 1011 }) : this.open()));
  }
  get machine() { return machineAt(this.url); }
  open() { this.readyState = 1; this.onopen?.(); }
  close() { this.readyState = 3; }
  reply(request, result) { this.onmessage?.({ data: JSON.stringify({ jsonrpc: "2.0", id: request.id, result }) }); }
  send(text) {
    const request = JSON.parse(text); this.sent.push(request);
    const id = this.machine;
    let result;
    switch (request.method) {
      case "daemon.info": result = { protocol_version: PROTOCOL_VERSION, environment_id: id, machine_id: id, version: "0.21.5", features: ["network.v1"] }; break;
      case "events.subscribe": result = { subscription_id: `s-${id}`, head_seq: 1, replay_ready: true }; break;
      case "providers.list": result = { providers: [] }; break;
      case "threads.list": result = { threads: [{ id: `t-${id}`, project_id: `p-${id}`, last_seq: 1, status: "idle", pinned: false, title: id, provider: { kind: "codex" }, updated_at: "2026-11-03T00:00:00Z" }] }; break;
      case "projects.list": result = { projects: [] }; break;
      case "approvals.list": result = { approvals: [] }; break;
      case "queue.list": result = { messages: [] }; break;
      case "settings.get": result = { default_provider: "codex", providers: {} }; break;
      case "spaces.list": result = { spaces: [{ id: "sp-1", name: "Work", color: "blue", position: 0, is_default: true }], placements: [] }; break;
      case "network.status": result = { enabled: true, self_machine: { ...card(id), is_self: true }, members: [card("mac"), card("vps")], found: [], outbox: {}, tailscale: {} }; break;
      // The network knows where the Mac listens now; it never lists the old daemon's port.
      case "network.routes": result = { routes: request.params.machine_id === "mac" ? [{ kind: "direct_tailnet", url: FRESH }] : [] }; break;
      default: result = {};
    }
    queueMicrotask(() => this.reply(request, result));
  }
}
const tick = async (n = 20) => { for (let i = 0; i < n; i++) await new Promise(setImmediate); };
const until = async (what, check, ms = 4000) => {
  const end = Date.now() + ms;
  while (!check()) {
    if (Date.now() > end) assert.fail(`timed out waiting for ${what}`);
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
};

const runtime = await import("../src/state/runtime.ts");
const { memberMachine, memberParity, olderMembers, canManageNetwork } = await import("../src/state/machineActions.ts");
const { homeReach } = await import("../src/state/homeReach.ts");
const { FAILOVER_AFTER_MS } = await import("../src/state/addressFailover.ts");
const spaces = await import("../src/state/spaces.ts");

test("a stale saved address: the Machines page still renders, Home works through the network, and the phone moves to the address the network knows", async (t) => {
  const previousSocket = globalThis.WebSocket;
  const realNow = Date.now;
  globalThis.WebSocket = Socket;
  t.after(() => {
    Date.now = realNow;
    unsubscribers.forEach((fn) => fn());
    runtime.connect(null);
    runtime.resetMachineHub();
    globalThis.WebSocket = previousSocket;
    globalThis.fetch = previousFetch;
  });
  await runtime.boot();
  const hub = runtime.machineHub();
  await until("kdi-vps to connect and the Mac's routes to be learned", () =>
    hub.connections().find((c) => c.record.machineId === "vps")?.status === "online" &&
    (hub.set.connection("mac")?.record.learned ?? []).some((route) => route.url === FRESH));
  const app = runtime.getState();
  assert.notEqual(app.status, "open", "the saved address never opens");
  assert.equal(hub.set.connection("mac").status, "offline", "so the Mac reads offline");
  assert.ok(sockets.every((s) => s.machine !== "mac" || s.url.includes(":59930")), "nothing tried the new address yet");

  // The Machines page (bug: `memberMachine` called `clientFor` on the offline Mac and threw while rendering).
  assert.doesNotThrow(() => memberMachine());
  assert.equal(memberMachine()?.machineId, "vps", "the live member answers for the network");
  assert.equal(memberParity(), true);
  assert.equal(canManageNetwork(), true);
  assert.deepEqual(olderMembers(), []);

  // Home: the Mac is away, kdi-vps answers, so Home is not the "Open connections" screen, and its spaces come from kdi-vps.
  const reach = homeReach(app.status, hub.view().machines);
  assert.deepEqual(reach, { connected: false, reachable: true });
  spaces.loadSpacesThroughNetwork();
  await tick();
  assert.deepEqual(spaces.useSpaces().spaces.map((s) => s.name), ["Work"]);
  assert.ok(sockets.find((s) => s.machine === "vps").sent.some((r) => r.method === "spaces.list"));

  // The saved address keeps failing: once it has for a while, the next retry tries the address the network reported.
  const start = realNow();
  Date.now = () => realNow() - start + start + FAILOVER_AFTER_MS + 1_000;
  await until("the phone to reconnect to the Mac on its new address", () => runtime.getState().status === "open", 20_000);
  const moved = runtime.getState().environments.find((e) => e.id === "mac");
  assert.equal(moved.url, "ws://mac.tail0745fc.ts.net:4175/ws");
  const fresh = sockets.filter((s) => s.machine === "mac" && s.url.includes(":4175"));
  assert.ok(fresh.length >= 1);
  assert.equal(fresh.at(-1).headers.Authorization, "Bearer tok-mac", "the Mac's own token, only to an address that proved it is the Mac");
  assert.equal(hub.set.connection("mac").status === "offline", false, "the Mac reads online again");
  // The saved computer keeps the old address after the new one, so a later move back is still possible.
  await hub.set.flush();
  const saved = JSON.parse(globalThis.__secure.get("kybern.ink.machines"));
  assert.deepEqual(saved.records.find((r) => r.machineId === "mac").routes.map((r) => r.url), [FRESH, STALE]);
  assert.deepEqual(homeReach(runtime.getState().status, hub.view().machines), { connected: true, reachable: true });
});
