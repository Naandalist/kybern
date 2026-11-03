import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { registerHooks } from "node:module";
import { fileURLToPath } from "node:url";
import test from "node:test";
registerHooks({ resolve(specifier, context, next) {
  const url = specifier.startsWith(".") && context.parentURL ? new URL(specifier, context.parentURL) : null;
  if (url?.protocol === "file:" && !/\.[a-z]+$/i.test(url.pathname) && existsSync(fileURLToPath(url) + ".ts")) return { shortCircuit: true, url: url.href + ".ts" };
  return next(specifier, context);
} });
const { homeReach } = await import("../src/state/homeReach.ts");
const { FAILOVER_AFTER_MS, FAILOVER_EVERY_MS, shouldTryOtherAddresses } = await import("../src/state/addressFailover.ts");

const mac = (status) => ({ status, isLocal: true });
const other = (status) => ({ status, isLocal: false });

test("Home works when the phone's computer answers, or when another machine of the network does", () => {
  assert.deepEqual(homeReach("open", [mac("online")]), { connected: true, reachable: true });
  assert.deepEqual(homeReach("reconnecting", []), { connected: false, reachable: false }, "nothing saved yet");
  assert.deepEqual(homeReach("reconnecting", [mac("offline")]), { connected: false, reachable: false }, "one computer, away");
  // The reported case: the Mac reads offline, kdi-vps and SatyaMas are up, instabox is away.
  const network = [mac("offline"), other("online"), other("offline"), other("needs-update")];
  assert.deepEqual(homeReach("reconnecting", network), { connected: false, reachable: true });
  assert.deepEqual(homeReach("failed", [mac("offline"), other("relaying")]), { connected: false, reachable: true });
  assert.deepEqual(homeReach("reconnecting", [mac("offline"), other("offline"), other("paused"), other("unpaired")]), { connected: false, reachable: false });
});

test("the other addresses are tried once the saved one has failed for a while, at most every few seconds, never for a refused credential", () => {
  const now = 1_000_000;
  const base = { status: "reconnecting", downSince: now - FAILOVER_AFTER_MS, lastTry: 0, trying: false, now };
  assert.equal(shouldTryOtherAddresses(base), true);
  assert.equal(shouldTryOtherAddresses({ ...base, downSince: now - FAILOVER_AFTER_MS + 1 }), false, "a short drop reconnects by itself");
  assert.equal(shouldTryOtherAddresses({ ...base, downSince: undefined }), false);
  assert.equal(shouldTryOtherAddresses({ ...base, status: "open" }), false);
  assert.equal(shouldTryOtherAddresses({ ...base, status: "closed" }), false);
  assert.equal(shouldTryOtherAddresses({ ...base, status: "idle" }), false);
  assert.equal(shouldTryOtherAddresses({ ...base, trying: true }), false, "one round at a time");
  assert.equal(shouldTryOtherAddresses({ ...base, lastTry: now - FAILOVER_EVERY_MS + 1 }), false);
  assert.equal(shouldTryOtherAddresses({ ...base, lastTry: now - FAILOVER_EVERY_MS }), true);
  assert.equal(shouldTryOtherAddresses({ ...base, status: "failed", downSince: now }), true, "a final failure of this address tries the others at once");
  assert.equal(shouldTryOtherAddresses({ ...base, status: "failed", failure: "auth" }), false, "the same computer refuses the same token");
});
