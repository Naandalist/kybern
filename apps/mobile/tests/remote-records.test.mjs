import assert from "node:assert/strict";
import test from "node:test";
import { mergeRemoteRecords } from "../src/state/remoteRecords.ts";

const record = (task, state, updated_at) => ({ task_id: task, state, updated_at });

test("remote records merge by task, and an older copy never replaces a newer one", () => {
  const empty = new Map();
  const running = record("a", "running", "2026-11-02T16:00:00Z");
  const first = mergeRemoteRecords(empty, [running]);
  assert.equal(first.get("a"), running);
  const done = record("a", "completed", "2026-11-02T18:06:00Z");
  const second = mergeRemoteRecords(first, [done, record("b", "pending", "2026-11-02T18:07:00Z")]);
  assert.equal(second.get("a"), done);
  assert.equal(second.size, 2);
  // A read answered after the notification still carries the running state: it is skipped.
  assert.equal(mergeRemoteRecords(second, [running]), second);
  assert.equal(mergeRemoteRecords(second, []), second);
  assert.equal(first.size, 1, "the earlier map is not changed");
});
