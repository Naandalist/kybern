// Adopting an app-owned connection, re-keying a machine, list activity and the
// refresh timeout (the gaps GPUIX's hub worked around in N6).
import test from "node:test";
import assert from "node:assert/strict";
import { MachineSet, RefreshTimeoutError, UnknownMachineError } from "../src/machines.ts";
import { MachineCache } from "../src/machineCache.ts";
import { FakeClient, fakeWorld, manualClock, memoryCacheStorage, memoryCredentials, memoryStore, verifyProof } from "./machine-fakes.mjs";

const rec = (machineId, routes = [], extra = {}) => ({ machineId, name: machineId, routes, member: false, ...extra });
const project = (id, name, root) => ({ id, name, path: `/w/${name}`, is_git: true, brief_in_all_threads: false, repo: { root_commit: root }, created_at: "2026-10-01T00:00:00Z", updated_at: "2026-10-01T00:00:00Z" });
const thread = (id, project_id, updated_at, extra = {}) => ({ id, project_id, title: id, updated_at, ...extra });

function setup({ records = [], credentials = {}, ...options } = {}) {
  const clock = manualClock();
  const world = fakeWorld(clock);
  const store = memoryStore(records);
  const creds = memoryCredentials(credentials);
  const storage = memoryCacheStorage();
  const cache = new MachineCache(storage);
  const set = new MachineSet({ store, credentials: creds, deviceId: "dev-1", verifyProof, transport: world.transport, clock, cache, ...options });
  return { clock, world, store, creds, storage, cache, set };
}

/** The app's own connection to a daemon: open, with `features` the daemon answered. */
function appClient(world, machineId, features = ["network.v1"]) {
  const host = { machineId, online: true, info: { machine_id: machineId, environment_id: machineId, version: "0.20.0", features } };
  const client = new FakeClient(world, host, { url: "ws://127.0.0.1:4173/ws" }, "app-token");
  client.info = host.info;
  client.setStatus("open");
  return client;
}

test("a late saved profile merges into a discovered member without losing routes, trust, lists or profile credentials", async () => {
  const ssh = { kind: "ssh", url: "http://127.0.0.1:5555" };
  const profile = { target: "me@box", remote_port: 4173, local_port: 5555 };
  const { set, world, creds, store } = setup({ records: [
    rec("legacy:box", [ssh], { legacyProfileId: "box", ssh: profile }),
    rec("box", [], { member: true, signingKey: "network-pin" }),
  ], credentials: { "legacy:box": "saved-token" } });
  await set.load();
  const discarded = [], deleted = [];
  creds.discard = (id) => { discarded.push(id); creds.tokens.delete(id); };
  creds.delete = (id) => { deleted.push(id); };
  const saved = appClient(world, "box"), discovered = appClient(world, "box");
  await set.adoptConnection(rec("legacy:box", [ssh], { legacyProfileId: "box", ssh: profile }), saved, { autoRefresh: false });
  await set.adoptConnection(rec("box", [], { member: true, signingKey: "network-pin" }), discovered, { autoRefresh: false });
  set.ingest("legacy:box", { projects: [project("p1", "Kybern", "root")], threads: [] });
  const proxy = set.clientFor("box");
  await set.mergeSavedMachine("legacy:box", "box");
  await set.flush();
  assert.deepEqual(set.connections().map((c) => c.record.machineId), ["box"]);
  assert.deepEqual(set.connection("box").record.routes, [ssh]);
  assert.equal(set.connection("box").record.legacyProfileId, "box");
  assert.deepEqual(store.records[0].ssh, profile);
  assert.equal(set.connection("box").record.signingKey, "network-pin");
  assert.equal(creds.tokens.get("box"), "saved-token");
  assert.equal(set.clientFor("box"), proxy);
  assert.deepEqual(discarded, ["legacy:box"]);
  assert.deepEqual(deleted, []);
  assert.equal(set.merged().projects[0].instances[0].machineId, "box");
  assert.equal(saved.closed, false, "the app-owned saved socket stays owned by the app");
  set.dispose();
});

test("adoptConnection joins an open client the set did not open, never dials it and never closes it", async () => {
  const { world, set, clock } = setup();
  const client = appClient(world, "mac");
  const connection = await set.adoptConnection(rec("mac"), client, { autoRefresh: false });
  assert.equal(connection.status, "online");
  assert.equal(connection.route.url, "http://127.0.0.1:4173");
  assert.equal(world.clients.length, 0);
  assert.equal(set.clientFor("mac"), connection.client);
  // The set shares the app's connection: calls go straight to it.
  world.on("mac", "projects.list", () => ({ projects: [] }));
  await connection.client.call("projects.list", {});
  assert.equal(client.calls.length, 1);

  // Leaving, pausing, being replaced or disposed never closes what the app owns.
  await set.setPaused("mac", true);
  assert.equal(set.connection("mac").status, "paused");
  assert.equal(client.closed, false);
  const next = appClient(world, "mac");
  const again = await set.adoptConnection(rec("mac"), next, { autoRefresh: false });
  assert.equal(again.status, "online");
  assert.equal(set.connection("mac").record.paused, false);
  const third = appClient(world, "mac");
  await set.adoptConnection(rec("mac"), third, { autoRefresh: false });
  assert.equal(next.closed, false);
  await set.removeMachine("mac");
  assert.equal(third.closed, false);

  const fourth = appClient(world, "mac");
  await set.adoptConnection(rec("mac"), fourth, { autoRefresh: false });
  set.dispose();
  assert.equal(fourth.closed, false);
  await clock.advance(10 * 60_000);
  assert.equal(world.clients.length, 0);
});

test("an owned adopted client is closed when the set lets go of it", async () => {
  const { world, set } = setup();
  const first = appClient(world, "mac");
  await set.adoptConnection(rec("mac"), first, { owned: true, autoRefresh: false });
  const second = appClient(world, "mac");
  await set.adoptConnection(rec("mac"), second, { owned: true, autoRefresh: false });
  assert.equal(first.closed, true);
  await set.removeMachine("mac");
  assert.equal(second.closed, true);
});

test("autoRefresh: false sends no list requests on adoption, reopening or ensureAll; refresh(id) still reads", async () => {
  const { world, set } = setup();
  world.lists("mac", [project("p1", "kybern", "r1")], [thread("t1", "p1", "2026-10-02T00:00:00Z")]);
  const client = appClient(world, "mac");
  await set.adoptConnection(rec("mac"), client, { autoRefresh: false });
  await set.ensureAll();
  client.setStatus("reconnecting");
  client.setStatus("open");
  await world.transport.probe("http://x").catch(() => {});
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(client.calls, []);
  assert.equal(set.merged().threads.all.length, 0);
  await set.refresh("mac");
  assert.deepEqual(client.calls.map(([method]) => method).sort(), ["projects.list", "threads.list"]);
  assert.equal(set.merged().threads.all.length, 1);
  set.dispose();
});

test("by default an adopted machine reads its lists on adoption and when it reopens", async () => {
  const { world, set } = setup();
  world.lists("mac", [project("p1", "kybern", "r1")], [thread("t1", "p1", "2026-10-02T00:00:00Z")]);
  const client = appClient(world, "mac");
  await set.adoptConnection(rec("mac"), client);
  await set.flush();
  assert.equal(client.calls.length, 2);
  assert.equal(set.merged().threads.all.length, 1);
  set.dispose();
});

test("lists given to adoptConnection are the machine's lists, cached, with no read", async () => {
  const { world, set, cache } = setup();
  const client = appClient(world, "mac");
  const lists = { projects: [project("p1", "kybern", "r1")], threads: [thread("t1", "p1", "2026-10-02T00:00:00Z")], activity: [{ thread_id: "t1" }] };
  await set.adoptConnection(rec("mac"), client, { lists, autoRefresh: false });
  assert.deepEqual(client.calls, []);
  assert.deepEqual(set.merged().threads.all.map((row) => row.thread.id), ["t1"]);
  assert.deepEqual(cache.get("mac").threads.map((row) => row.id), ["t1"]);
  assert.deepEqual(set.activityOf("mac"), [{ thread_id: "t1" }]);
  set.dispose();
});

test("an adopted connection that drops is offline, and the set neither dials nor retries it", async () => {
  const { world, set, clock } = setup();
  const client = appClient(world, "mac");
  await set.adoptConnection(rec("mac"), client, { autoRefresh: false });
  client.setStatus("reconnecting");
  assert.equal(set.connection("mac").status, "offline");
  await clock.advance(60_000);
  client.setStatus("failed");
  await clock.advance(10 * 60_000);
  assert.equal(world.clients.length, 0);
  assert.throws(() => set.clientFor("mac"));
  // The app reopens its client and the set follows it.
  client.setStatus("open");
  assert.equal(set.connection("mac").status, "online");
  set.dispose();
});

test("an adopted client that is not open yet is listed offline until it opens", async () => {
  const { world, set } = setup();
  const client = appClient(world, "mac");
  client.setStatus("connecting");
  const connection = await set.adoptConnection(rec("mac"), client, { autoRefresh: false });
  assert.equal(connection.status, "offline");
  client.setStatus("open");
  assert.equal(set.connection("mac").status, "online");
  set.dispose();
});

test("rekeyMachine moves record, cache, connection and token in one step with one change", async () => {
  const { world, set, store, creds, cache } = setup({ records: [rec("legacy:a"), rec("legacy:local", [], { legacyProfileId: "local" }), rec("legacy:c")], credentials: { "legacy:local": "tok" } });
  await set.ensureAll();
  set.setLocalMachine("legacy:local");
  set.ingest("legacy:local", { projects: [project("p1", "kybern", "r1")], threads: [thread("t1", "p1", "2026-10-02T00:00:00Z")] });
  const client = appClient(world, "mac");
  await set.adoptConnection(rec("legacy:local"), client, { autoRefresh: false });
  set.ingest("legacy:local", { projects: [project("p1", "kybern", "r1")], threads: [thread("t1", "p1", "2026-10-02T00:00:00Z")] });
  assert.ok(cache.get("legacy:local"));
  const handle = set.handleFor("legacy:local");
  const events = [];
  set.onChange((event) => events.push(event));
  const view = await set.rekeyMachine("legacy:local", "mac");
  await set.flush();

  assert.deepEqual(events, [{ type: "records", machineId: "mac" }]);
  assert.equal(view.record.machineId, "mac");
  assert.deepEqual(set.connections().map((c) => c.record.machineId), ["mac", "legacy:a", "legacy:c"]);
  assert.equal(set.localMachineId, "mac");
  assert.equal(set.connection("legacy:local"), undefined);
  assert.equal(set.connection("mac").status, "online");
  assert.equal(set.clientFor("mac"), handle);
  assert.equal(handle.machineId, "mac");
  assert.equal(handle.endpoint.environmentId, "mac");
  assert.deepEqual(set.merged().threads.all.map((row) => [row.machineId, row.thread.id]), [["mac", "t1"]]);
  assert.equal(set.machineOfThread("t1"), "mac");
  assert.equal(cache.get("legacy:local"), undefined);
  assert.deepEqual(cache.get("mac").threads.map((row) => row.id), ["t1"]);
  assert.equal(creds.tokens.get("mac"), "tok");
  assert.equal(creds.tokens.has("legacy:local"), false);
  assert.deepEqual(store.records.map((r) => r.machineId), ["legacy:a", "mac", "legacy:c"]);
  assert.ok(store.records.some((r) => r.machineId === "mac" && r.legacyProfileId === "local"));
  assert.ok(!store.records.some((r) => r.machineId === "legacy:local"));
  set.dispose();
});

test("rekeyMachine keeps subscriptions made before the new id was known", async () => {
  const { world, set } = setup({ records: [rec("legacy:local")] });
  await set.ensureAll();
  const handle = set.handleFor("legacy:local");
  const got = [];
  handle.subscribeEvents({ thread_id: "t1" }, (event) => got.push(event.seq));
  const client = appClient(world, "mac");
  await set.adoptConnection(rec("legacy:local"), client, { autoRefresh: false });
  await set.rekeyMachine("legacy:local", "mac");
  client.emit(5);
  assert.deepEqual(got, [5]);
  set.dispose();
});

test("rekeyMachine refuses an unknown machine or a taken id and changes nothing", async () => {
  const { set, creds } = setup({ records: [rec("legacy:a"), rec("b")], credentials: { "legacy:a": "tok-a", b: "tok-b" } });
  await set.ensureAll();
  const before = set.version;
  await assert.rejects(set.rekeyMachine("nope", "x"), UnknownMachineError);
  await assert.rejects(set.rekeyMachine("legacy:a", "b"), /already in your list/);
  assert.equal(set.version, before);
  assert.equal(creds.tokens.get("legacy:a"), "tok-a");
  assert.equal(creds.tokens.get("b"), "tok-b");
  assert.deepEqual(set.connections().map((c) => c.record.machineId), ["legacy:a", "b"]);
  // The same id is a no-op.
  assert.equal((await set.rekeyMachine("b", "b")).record.machineId, "b");
  assert.equal(set.version, before);
  set.dispose();
});

test("rekeyMachine changes nothing when the token cannot be saved under the new id", async () => {
  const { set, creds, cache } = setup({ records: [rec("legacy:a")], credentials: { "legacy:a": "tok-a" } });
  await set.ensureAll();
  set.ingest("legacy:a", { threads: [thread("t1", "p1", "2026-10-02T00:00:00Z")] });
  creds.set = () => {
    throw new Error("keychain locked");
  };
  await assert.rejects(set.rekeyMachine("legacy:a", "a"), /keychain locked/);
  assert.deepEqual(set.connections().map((c) => c.record.machineId), ["legacy:a"]);
  assert.equal(creds.tokens.get("legacy:a"), "tok-a");
  assert.equal(cache.get("a"), undefined);
  set.dispose();
});

test("a list refresh keeps the activity threads.list reported, and ingest keeps it until new threads arrive", async () => {
  const { world, set } = setup({ records: [rec("b", [{ kind: "direct_advertised", url: "https://b.example.com" }], { member: false })], credentials: { b: "tok" } });
  world.host("https://b.example.com", "b");
  const activity = [{ thread_id: "t1", line: "Using Bash" }];
  world.on("b", "projects.list", () => ({ projects: [project("p1", "kybern", "r1")] }));
  world.on("b", "threads.list", () => ({ threads: [thread("t1", "p1", "2026-10-02T00:00:00Z", { extra: 1 })], activity }));
  await set.ensureAll();
  assert.deepEqual(set.activityOf("b"), activity);
  assert.equal(set.merged().threads.all[0].thread.extra, 1);
  // Projects alone do not forget what the threads said.
  set.ingest("b", { projects: [project("p1", "kybern", "r1")] });
  assert.deepEqual(set.activityOf("b"), activity);
  set.ingest("b", { threads: [thread("t1", "p1", "2026-10-02T00:00:00Z")] });
  assert.equal(set.activityOf("b"), undefined);
  assert.equal(set.activityOf("nope"), undefined);
  set.dispose();
});

function hangingMachine(options = {}) {
  const ctx = setup({ records: [rec("b", [{ kind: "direct_advertised", url: "https://b.example.com" }])], credentials: { b: "tok" }, ...options });
  ctx.world.host("https://b.example.com", "b");
  ctx.world.lists("b", [project("p1", "kybern", "r1")], [thread("t1", "p1", "2026-10-02T00:00:00Z")]);
  return ctx;
}

test("refresh of a machine whose lists never arrive rejects after the timeout and leaves its lists alone", async () => {
  const { world, set, clock, cache } = hangingMachine();
  await set.ensureAll();
  const before = cache.get("b");
  assert.deepEqual(set.merged().threads.all.map((row) => row.thread.id), ["t1"]);
  world.on("b", "threads.list", () => new Promise(() => {}));
  const pending = set.refresh("b");
  const outcome = pending.then(() => "resolved", (error) => error);
  await clock.advance(29_000);
  assert.equal(await Promise.race([outcome, Promise.resolve("still waiting")]), "still waiting");
  await clock.advance(2_000);
  const error = await outcome;
  assert.ok(error instanceof RefreshTimeoutError);
  assert.equal(error.machineId, "b");
  assert.match(error.message, /did not send its lists in time/);
  assert.deepEqual(set.merged().threads.all.map((row) => row.thread.id), ["t1"]);
  assert.equal(cache.get("b"), before);
  assert.equal(set.connection("b").error.code, "refresh_failed");
  // Later reads work again.
  world.on("b", "threads.list", () => ({ threads: [thread("t2", "p1", "2026-10-03T00:00:00Z")] }));
  await set.refresh("b");
  assert.deepEqual(set.merged().threads.all.map((row) => row.thread.id), ["t2"]);
  set.dispose();
});

test("the refresh timeout is configurable, a late answer is dropped, and a whole-set refresh does not reject", async () => {
  const { world, set, clock } = hangingMachine({ refreshTimeoutMs: 1_000 });
  await set.ensureAll();
  let answer;
  world.on("b", "threads.list", () => new Promise((resolve) => (answer = resolve)));
  const one = set.refresh("b").then(() => "resolved", (error) => error);
  await clock.advance(1_001);
  assert.ok((await one) instanceof RefreshTimeoutError);
  answer({ threads: [thread("late", "p1", "2026-10-09T00:00:00Z")] });
  await clock.flush();
  assert.deepEqual(set.merged().threads.all.map((row) => row.thread.id), ["t1"]);

  world.on("b", "threads.list", () => new Promise(() => {}));
  const all = set.refresh();
  await clock.advance(1_001);
  await all; // settles without rejecting
  set.dispose();
});

test("a list read that hangs while connecting does not stop ensureAll or leave an unhandled rejection", async () => {
  const { world, set, clock } = hangingMachine({ refreshTimeoutMs: 500 });
  world.on("b", "projects.list", () => new Promise(() => {}));
  const done = set.ensureAll();
  await clock.advance(1_000);
  await done;
  assert.equal(set.connection("b").status, "online");
  assert.equal(set.connection("b").error.code, "refresh_failed");
  set.dispose();
});

test("rekeyMachine copies the token and asks a store that keeps an older app's copy to discard, not delete, the stand-in", async () => {
  const { set, creds } = setup({ records: [rec("legacy:a", [], { legacyProfileId: "p-a" })], credentials: { "legacy:a": "tok" } });
  const discarded = [];
  creds.discard = (id) => void discarded.push(id);
  const deleted = [];
  const realDelete = creds.delete;
  creds.delete = (id) => (deleted.push(id), realDelete(id));
  await set.ensureAll();
  await set.rekeyMachine("legacy:a", "a");
  assert.equal(creds.tokens.get("a"), "tok");
  assert.deepEqual(discarded, ["legacy:a"]);
  assert.deepEqual(deleted, []);
  set.dispose();
});

test("removeMachine deletes the token through delete (a store removes every copy then)", async () => {
  const { set, creds } = setup({ records: [rec("a")], credentials: { a: "tok" } });
  creds.discard = () => assert.fail("forgetting a machine is not a discard");
  await set.ensureAll();
  await set.removeMachine("a");
  assert.equal(creds.tokens.has("a"), false);
  set.dispose();
});

test("alternateRoutes: the other direct addresses of an app-owned machine that prove they are it, never the one in use, an impostor, a blocked one, SSH or a relay", async () => {
  const STALE = "http://mac.tail.ts.net:59930";
  const FRESH = "http://mac.tail.ts.net:4175";
  const LAN = "http://192.168.1.5:4175";
  const OTHER = "http://100.64.0.9:4173";
  const BLOCKED = "http://100.64.0.10:4173";
  const { set, world } = setup({ records: [rec("mac", [{ kind: "direct_advertised", url: STALE }], { member: true, signingKey: "key-mac", blockedRoutes: [`direct_tailnet|${BLOCKED}|`] })], credentials: { mac: "tok" } });
  // An old daemon still answers as the Mac at the saved address; the live one moved; something else sits on OTHER.
  world.host(STALE, "mac");
  world.host(FRESH, "mac");
  world.host(LAN, "mac", { online: false });
  world.host(OTHER, "mac", { answerAs: { machineId: "intruder", key: "key-intruder" } });
  world.host(BLOCKED, "mac");
  await set.adoptConnection({ machineId: "mac", name: "Mac" }, appClient(world, "mac"), { autoRefresh: false });
  // What a member reported for the Mac (`network.routes`).
  set["learn"](set["slots"].get("mac"), [
    { kind: "direct_tailnet", url: FRESH },
    { kind: "direct_tailnet", url: OTHER },
    { kind: "direct_tailnet", url: BLOCKED },
    { kind: "direct_advertised", url: LAN },
    { kind: "ssh", url: "http://127.0.0.1:5555" },
    { kind: "relay", url: "http://vps.tail.ts.net:4173/relay/mac", via: "vps" },
  ]);
  world.probes.length = 0;
  const routes = await set.alternateRoutes("mac", ["ws://mac.tail.ts.net:59930/ws"]);
  assert.deepEqual(routes.map((route) => route.url), [FRESH]);
  assert.ok(!world.probes.some((url) => url.startsWith(STALE)), "the address in use is not probed");
  assert.ok(!world.probes.some((url) => url.startsWith(BLOCKED)), "a blocked address is not probed");
  assert.equal(world.clients.length, 0, "nothing was opened and no token was sent");
  assert.deepEqual(await set.alternateRoutes("nobody"), []);
  // Without `except` the saved address counts too: it still proves it is the Mac.
  assert.deepEqual((await set.alternateRoutes("mac")).map((route) => route.url), [FRESH, STALE]);
  set.dispose();
});
