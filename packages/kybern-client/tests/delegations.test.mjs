import test from "node:test";
import assert from "node:assert/strict";
import {
  canRemoveWorktree,
  canStopDelegation,
  childState,
  conflictPaths,
  delegationMeta,
  delegationStatusWord,
  descendantsOf,
  diffstatLabel,
  heldHeadline,
  heldMessages,
  launchCounts,
  launchGroupDefaultOpen,
  launchGroupTitle,
  launchState,
  isRemoteChildSettled,
  remoteChildState,
  remoteDelegationStatus,
  remoteRecordWins,
  withRemoteRecords,
  lineageCounts,
  lineageKind,
  lineageRows,
  lineageSummary,
  messageSenderName,
  plainLine,
  purposeLabel,
  resultNeedsClamp,
  resultPreview,
  shortBranch,
  structuredMessageBody,
  structuredMessageSummary,
  workspaceLabel,
} from "../src/delegations.ts";
import {
  delegateTitle,
  findToolJson,
  isDelegateTool,
  orchestrationLabel,
  orchestrationTool,
  parseDelegateInput,
  parseDelegateResult,
  parseSendInput,
  parseSendResult,
  sendStateWord,
} from "../src/orchestrationTools.ts";
import { isChildThread, visibleSubagentThreads } from "../src/subagents.ts";

const T0 = Date.parse("2026-10-06T10:00:00Z");
const iso = (seconds) => new Date(T0 + seconds * 1000).toISOString();

function delegated(id, status, extra = {}, thread = {}) {
  return {
    id,
    title: id,
    parent_thread_id: "root",
    status: "idle",
    created_at: iso(0),
    delegation: { status, workspace: "shared", started_at: iso(0), ...extra },
    ...thread,
  };
}
const native = (id, status, thread = {}) => ({ id, title: id, parent_thread_id: "root", status: "idle", created_at: iso(0), subagent: { status, started_at: iso(0) }, ...thread });
const helper = (id, thread = {}) => ({ id, title: id, parent_thread_id: "root", status: "idle", created_at: iso(0), ...thread });

test("status is a word and a phase, and interrupted keeps its own word", () => {
  assert.equal(delegationStatusWord("running"), "Working");
  assert.equal(delegationStatusWord("cancelled"), "Stopped");
  assert.equal(delegationStatusWord("interrupted"), "Interrupted");
  assert.deepEqual(childState(delegated("a", "completed")), { phase: "done", word: "Done" });
  assert.deepEqual(childState(delegated("a", "interrupted")), { phase: "stopped", word: "Interrupted" });
  assert.deepEqual(childState(delegated("a", "running")), { phase: "working", word: "Working" });
});

test("a running child waiting on an approval says so instead of working", () => {
  assert.deepEqual(childState(delegated("a", "running", {}, { status: "awaiting-approval" })), { phase: "waiting", word: "Needs approval" });
});

test("native subagents and legacy helpers read from their own records", () => {
  assert.deepEqual(childState(native("n", "running")), { phase: "working", word: "Working" });
  assert.deepEqual(childState(native("n", "pending")), { phase: "working", word: "Starting" });
  assert.deepEqual(childState(native("n", "failed")), { phase: "failed", word: "Failed" });
  assert.deepEqual(childState(native("n", "stopped")), { phase: "stopped", word: "Stopped" });
  assert.deepEqual(childState(helper("h", { status: "running" })), { phase: "working", word: "Working" });
  assert.deepEqual(childState(helper("h")), { phase: "idle", word: "Idle" });
  assert.deepEqual(childState(helper("h", { status: "failed" })), { phase: "failed", word: "Failed" });
});

test("kinds, stop and remove rules", () => {
  assert.equal(lineageKind(delegated("a", "running")), "delegated");
  assert.equal(lineageKind(native("n", "running")), "native");
  assert.equal(lineageKind(helper("h")), "helper");
  assert.equal(canStopDelegation(delegated("a", "running")), true);
  assert.equal(canStopDelegation(delegated("a", "completed")), false);
  assert.equal(canStopDelegation(native("n", "running")), false, "harness-native children stop through tasks.stop, not Lineage");
  assert.equal(canRemoveWorktree({ workspace: "worktree", worktree_state: "kept" }), true);
  assert.equal(canRemoveWorktree({ workspace: "worktree", worktree_state: "active" }), false);
  assert.equal(canRemoveWorktree({ workspace: "shared", worktree_state: null }), false);
  assert.equal(canRemoveWorktree(null), false);
});

test("workspace labels use the plain words", () => {
  assert.equal(workspaceLabel({ workspace: "shared" }), "Shared checkout");
  assert.equal(workspaceLabel({ workspace: "worktree", worktree_state: "active" }), "Own worktree");
  assert.equal(workspaceLabel({ workspace: "worktree", worktree_state: "kept" }), "Worktree kept");
  assert.equal(workspaceLabel({ workspace: "worktree", worktree_state: "removed" }), "Worktree removed");
  assert.equal(shortBranch("kybern/4f2a91c3-77aa-4c1d-9b2e-0123456789ab"), "kybern/4f2a91c3");
  assert.equal(shortBranch("feature/login"), "feature/login");
  assert.equal(diffstatLabel({ files: 1, additions: 12, deletions: 0 }), "1 file · +12 −0");
  assert.deepEqual(conflictPaths({ conflicts: [{ path: "a.ts" }, { path: "a.ts" }, { path: "b.ts" }] }), ["a.ts", "b.ts"]);
});

test("plainLine strips markdown and cuts at a word", () => {
  assert.equal(plainLine("## **Done**: see [the PR](https://x.test/1)\nmore"), "Done: see the PR");
  assert.equal(plainLine("```\ncode\n```\nReal line"), "code");
  assert.equal(plainLine("Added the `0012_add_sign_in_method` migration"), "Added the 0012_add_sign_in_method migration", "snake_case identifiers keep their underscores");
  assert.equal(plainLine("Is _this_ and __that__ done?"), "Is this and that done?");
  assert.equal(plainLine(""), "");
  assert.equal(plainLine(null), "");
  const long = "word ".repeat(60);
  const cut = plainLine(long, 40);
  assert.ok(cut.endsWith("…") && cut.length <= 41, cut);
  assert.ok(!/\bwor…$/.test(cut), "cuts on a word boundary");
});

test("a failed result leads with its error", () => {
  assert.equal(resultPreview({ status: "failed", result: "partial", error: "Tests failed: 3" }), "Tests failed: 3");
  assert.equal(resultPreview({ status: "completed", result: "Added the form.\nDetails", error: null }), "Added the form.");
  assert.equal(resultPreview({ status: "interrupted", result: null, error: "interrupted by a Kybern restart" }), "interrupted by a Kybern restart");
  assert.equal(resultPreview({ status: "completed", result: null, error: null }), "");
  assert.equal(resultNeedsClamp("short"), false);
  assert.equal(resultNeedsClamp("a\n".repeat(9)), true);
  assert.equal(resultNeedsClamp("x".repeat(800)), true);
});

test("lineage rows nest children and grandchildren in launch order", () => {
  const a = delegated("a", "completed", { started_at: iso(10) });
  const b = delegated("b", "running", { started_at: iso(5) });
  const grand = helper("g", { parent_thread_id: "b", status: "running", created_at: iso(6) });
  const rows = lineageRows([a, b, grand], "root");
  assert.deepEqual(rows.map((row) => [row.thread.id, row.depth, row.kind]), [["b", 0, "delegated"], ["g", 1, "helper"], ["a", 0, "delegated"]]);
  assert.equal(rows[0].childCount, 1);
});

test("a settled branch starts closed, an active one open, and the reader's choice wins", () => {
  const done = delegated("d", "completed", { started_at: iso(1) });
  const doneKid = native("dk", "completed", { parent_thread_id: "d" });
  const live = delegated("l", "running", { started_at: iso(2) });
  const liveKid = native("lk", "completed", { parent_thread_id: "l" });
  const all = [done, doneKid, live, liveKid];
  assert.deepEqual(lineageRows(all, "root").map((row) => row.thread.id), ["d", "l", "lk"]);
  assert.deepEqual(lineageRows(all, "root", { expanded: { d: true, l: false } }).map((row) => row.thread.id), ["d", "dk", "l"]);
});

test("lineage skips archived threads, the root itself and cycles", () => {
  const a = delegated("a", "running");
  const gone = helper("x", { status: "archived" });
  const loop1 = helper("p", { parent_thread_id: "q" });
  const loop2 = helper("q", { parent_thread_id: "p" });
  const self = helper("root", { parent_thread_id: "root" });
  assert.deepEqual(lineageRows([a, gone, loop1, loop2, self], "root").map((row) => row.thread.id), ["a"]);
});

test("descendants come in tree order and skip archived threads and cycles", () => {
  const a = delegated("a", "running", { started_at: iso(2) });
  const b = delegated("b", "running", { started_at: iso(1) });
  const kid = helper("kid", { parent_thread_id: "b", created_at: iso(3) });
  const gone = helper("gone", { parent_thread_id: "b", status: "archived" });
  const loop = helper("root", { parent_thread_id: "kid" });
  assert.deepEqual(descendantsOf([a, b, kid, gone, loop], "root").map((thread) => thread.id), ["b", "kid", "a"]);
  assert.deepEqual(descendantsOf([], "root"), []);
});

test("depth beyond the cap hides further children", () => {
  const chain = [delegated("a", "running"), helper("b", { parent_thread_id: "a", status: "running" }), helper("c", { parent_thread_id: "b", status: "running" })];
  assert.deepEqual(lineageRows(chain, "root", { maxDepth: 1 }).map((row) => row.thread.id), ["a", "b"]);
});

test("counts include collapsed branches and the summary skips zeros and idle helpers", () => {
  const all = [delegated("a", "running"), delegated("b", "completed"), delegated("c", "failed"), native("n", "completed", { parent_thread_id: "c" }), helper("h")];
  const counts = lineageCounts(all, "root");
  assert.deepEqual(counts, { working: 1, waiting: 0, done: 2, failed: 1, stopped: 0, idle: 1, total: 5 });
  assert.equal(lineageSummary(counts), "1 working · 2 done · 1 failed");
  assert.equal(lineageSummary(lineageCounts([], "root")), "");
});

test("launch rows fall back to the call until the child thread is known", () => {
  assert.deepEqual(launchState(undefined, { complete: false, isError: false }), { phase: "working", word: "Starting" });
  assert.deepEqual(launchState(undefined, { complete: true, isError: true }), { phase: "failed", word: "Failed" });
  assert.deepEqual(launchState(delegated("a", "completed"), { complete: true, isError: false }), { phase: "done", word: "Done" });
  const counts = launchCounts(["working", "waiting", "done", "failed", "stopped", "idle"]);
  assert.deepEqual(counts, { working: 2, done: 2, failed: 1, stopped: 1 });
  assert.equal(launchGroupTitle(3), "3 delegated agents");
  assert.equal(launchGroupDefaultOpen({ working: 2, done: 0, failed: 0, stopped: 0 }), true);
  assert.equal(launchGroupDefaultOpen({ working: 4, done: 0, failed: 0, stopped: 0 }), false);
  assert.equal(launchGroupDefaultOpen({ working: 0, done: 3, failed: 0, stopped: 0 }), false);
  assert.equal(launchGroupDefaultOpen({ working: 0, done: 2, failed: 1, stopped: 0 }), true);
});

test("inbound message vocabulary", () => {
  assert.equal(purposeLabel("question"), "Question");
  assert.equal(messageSenderName({ from_thread_id: null, from_title: "" }), "Kybern");
  assert.equal(messageSenderName({ from_thread_id: "t", from_title: "Review sign-in" }), "Review sign-in");
  assert.equal(messageSenderName({ from_thread_id: "t", from_title: " " }), "Another thread");
  assert.equal(structuredMessageSummary({ parts: [{ type: "thread_message", purpose: "warning", from_thread_id: null, from_title: "", body: "x", message_id: "m" }] }), "Warning from Kybern");
  assert.equal(structuredMessageSummary({ parts: [{ type: "agent_results", items: [{}, {}] }] }), "Results from 2 agents");
  assert.equal(structuredMessageSummary({ parts: [{ type: "agent_results", items: [{}] }] }), "Result from 1 agent");
  assert.equal(structuredMessageSummary({ parts: [{ type: "text", text: "hi" }] }), null);
  assert.equal(heldHeadline("Review sign-in", "question"), "Review sign-in wants to send a question");
  assert.equal(heldHeadline("Kybern", "warning"), "Kybern wants to send a warning");
});

test("a queued structured message reads as text", () => {
  assert.equal(structuredMessageBody({ parts: [{ type: "thread_message", purpose: "message", from_thread_id: "t", from_title: "A", body: "Hello", message_id: "m" }] }), "Hello");
  const results = structuredMessageBody({ parts: [{ type: "agent_results", items: [{ title: "Form", status: "completed", result: "Added it.", error: null }, { title: "Tests", status: "failed", result: null, error: "3 failed" }] }] });
  assert.equal(results, "Form: Done — Added it.\nTests: Failed — 3 failed");
  assert.equal(structuredMessageBody({ parts: [{ type: "text", text: "x" }] }), null);
});

test("held messages keep only held ones, oldest first", () => {
  const messages = [
    { id: "b", state: "held", created_at: iso(20) },
    { id: "a", state: "held", created_at: iso(10) },
    { id: "c", state: "delivered", created_at: iso(5) },
  ];
  assert.deepEqual(heldMessages(messages).map((message) => message.id), ["a", "b"]);
});

test("child threads of either kind follow the sidebar visibility rule", () => {
  const now = T0 + 100_000;
  const none = new Set();
  const working = delegated("working", "running", { started_at: iso(30) });
  const longDone = delegated("long", "completed", { started_at: iso(0), completed_at: iso(5) });
  const justDone = delegated("just", "completed", { started_at: iso(0), completed_at: iso(99) });
  const sub = native("sub", "running");
  const ordinary = helper("ordinary");
  const ids = (selectedId, dismissed = none) => visibleSubagentThreads([working, longDone, justDone, sub, ordinary], { selectedId, dismissed, now }).map((thread) => thread.id).sort();
  assert.deepEqual(ids(null), ["just", "sub", "working"], "settled long ago and ordinary threads stay out");
  assert.deepEqual(ids("long"), ["just", "long", "sub", "working"]);
  assert.deepEqual(ids(null, new Set(["working"])), ["just", "sub"]);
  assert.equal(isChildThread(working), true);
  assert.equal(isChildThread(sub), true);
  assert.equal(isChildThread(ordinary), false);
});

test("a delegated parent stays while its delegated child is listed", () => {
  const now = T0 + 100_000;
  const parent = delegated("parent", "completed", { started_at: iso(0), completed_at: iso(10) });
  const nested = delegated("nested", "running", { started_at: iso(5) }, { parent_thread_id: "parent" });
  assert.deepEqual(visibleSubagentThreads([parent, nested], { selectedId: null, dismissed: new Set(), now }).map((thread) => thread.id), ["parent", "nested"]);
});

// ---- tools ----

test("orchestration tools are recognised under every harness spelling", () => {
  assert.equal(orchestrationTool("mcp__kybern__kybern_agent_delegate"), "agent_delegate");
  assert.equal(orchestrationTool("kybern_thread_send"), "thread_send");
  assert.equal(orchestrationTool("Kybern_Thread_Interrupt"), "thread_interrupt");
  assert.equal(orchestrationTool("mcp__kybern__kybern_thread_read"), null);
  assert.equal(orchestrationTool("notkybern_agent_status"), null);
  assert.equal(isDelegateTool("mcp__kybern__kybern_agent_delegate"), true);
  assert.equal(isDelegateTool("kybern_agent_status"), false);
});

test("delegate input and result are read through any envelope", () => {
  assert.deepEqual(parseDelegateInput({ task: "Fix it", role: "review", workspace: "worktree", provider: "codex" }), {
    task: "Fix it", title: null, role: "review", provider: "codex", model: null, workspace: "worktree", operationId: null,
  });
  assert.equal(parseDelegateInput({ request_key: "k1" }).operationId, "k1");
  assert.equal(parseDelegateInput({ role: "bogus" }).role, null);
  const body = { task_id: "t1", thread_id: "th1", title: "Add form", provider: "codex", model: "gpt-5", workspace: "worktree", branch: "kybern/ab", status: "running" };
  for (const output of [body, JSON.stringify(body), [{ type: "text", text: JSON.stringify(body) }], { content: [{ type: "text", text: JSON.stringify(body) }] }, { result: JSON.stringify(body) }]) {
    const parsed = parseDelegateResult(output);
    assert.equal(parsed?.taskId, "t1");
    assert.equal(parsed?.branch, "kybern/ab");
    assert.equal(parsed?.status, "running");
  }
  assert.equal(parseDelegateResult({ message: "nothing" }), null);
  assert.equal(parseDelegateResult(null), null);
  assert.equal(parseDelegateResult("not json"), null);
  assert.equal(delegateTitle({ task: "Write the migration\nthen test it" }, null), "Write the migration");
  assert.equal(delegateTitle({ task: "x", title: "Mine" }, null), "Mine");
  assert.equal(delegateTitle({ task: "x", title: "Mine" }, parseDelegateResult(body)), "Add form");
});

test("send input and result drive the Sent-to row", () => {
  assert.deepEqual(parseSendInput({ thread_id: "t", body: "Hi", purpose: "question", delivery: "steer", wait_for_reply: true }), {
    threadId: "t", body: "Hi", purpose: "question", replyTo: null, steer: true, waitForReply: true,
  });
  assert.equal(parseSendInput({ purpose: "task" }).purpose, "message");
  const held = parseSendResult({ message_id: "m", state: "held", delivered_as: "held" });
  assert.equal(held?.state, "held");
  assert.equal(sendStateWord(held, true, false), "Held for approval");
  const answered = parseSendResult(JSON.stringify({ message_id: "m", state: "answered", delivered_as: "queued", reply: { message_id: "r", from_thread_id: "x", body: "Yes" } }));
  assert.equal(answered?.reply?.body, "Yes");
  assert.equal(sendStateWord(answered, true, false), "Answered");
  assert.equal(sendStateWord(parseSendResult({ message_id: "m", delivered_as: "steered" }), true, false), "Sent now");
  assert.equal(sendStateWord(parseSendResult({ message_id: "m", delivered_as: "queued" }), true, false), "Queued");
  assert.equal(sendStateWord(null, false, false), "Sending");
  assert.equal(sendStateWord(null, true, true), "Not sent");
  assert.equal(parseSendResult({ state: "queued" }), null, "no message id, no result");
});

test("quiet tools get a tense-aware one-line label", () => {
  assert.equal(orchestrationLabel("agent_wait", false, false), "Waiting for delegated agents");
  assert.equal(orchestrationLabel("agent_wait", true, false), "Waited for delegated agents");
  assert.equal(orchestrationLabel("agent_cancel", true, false), "Stopped a delegated agent");
  assert.equal(orchestrationLabel("agent_status", true, true), "Unable to check delegated agents");
  assert.equal(orchestrationLabel("thread_interrupt", false, false), "Stopping a thread");
  assert.equal(orchestrationLabel("agent_capabilities", true, false), "Listed available agents");
});

test("findToolJson stops on cycles and deep nesting", () => {
  const cyclic = { content: [] };
  cyclic.content.push(cyclic);
  assert.equal(findToolJson(cyclic, () => null), null);
  let deep = { task_id: "t", thread_id: "x" };
  for (let i = 0; i < 12; i++) deep = { result: deep };
  assert.equal(parseDelegateResult(deep), null);
});

test("delegationMeta names the model as the picker does and drops the default workspace", () => {
  const providers = [
    { kind: "claude-code", models: [{ id: "sonnet", resolved_id: "claude-sonnet-5-5", display_name: "Sonnet 5.5" }] },
    { kind: "codex", models: [{ id: "gpt-6.1-sol", display_name: "GPT-6.1 Sol" }] },
  ];
  assert.equal(delegationMeta("claude-sonnet-5-5", "claude-code", providers, "Shared checkout"), "Sonnet 5.5");
  assert.equal(delegationMeta("sonnet", "claude-code", providers, "Shared checkout"), "Sonnet 5.5");
  assert.equal(delegationMeta("gpt-6.1-sol", "codex", providers, "Own worktree"), "GPT-6.1 Sol · Own worktree");
  assert.equal(delegationMeta("mystery-1", "pi", providers, null), "mystery-1");
  assert.equal(delegationMeta(null, "codex", providers, "Worktree kept"), "Worktree kept");
  assert.equal(delegationMeta("sonnet", "claude-code", providers, "Shared checkout", "Windows PC"), "Sonnet 5.5 · on Windows PC");
  assert.equal(delegationMeta("gpt-6.1-sol", "codex", providers, "Own worktree", "instabox"), "GPT-6.1 Sol · on instabox · Own worktree");
});

// ---- ADE-121: children on other machines, by the delegating machine's own record ----

const remoteRecord = (task, state, extra = {}) => ({
  task_id: task,
  operation_id: `op-${task}`,
  parent_thread_id: "root",
  target: "satyamas",
  remote_thread_id: `t-${task}`,
  state,
  last_seq: 1,
  title: `Child ${task}`,
  info: { task_id: task, operation_id: `op-${task}`, parent_thread_id: "root", depth: 1, role: "implementation", workspace: "shared", status: remoteDelegationStatus(state), started_at: iso(0), ...(state === "completed" ? { completed_at: iso(60), result: "All done" } : {}) },
  created_at: iso(0),
  updated_at: iso(60),
  ...extra,
});

test("a launch row whose remote child is unknown here follows the delegating machine's record", () => {
  const call = { complete: true, isError: false };
  assert.deepEqual(launchState(undefined, call), { phase: "working", word: "Working" });
  assert.deepEqual(launchState(undefined, call, remoteRecord("a", "completed")), { phase: "done", word: "Done" });
  assert.deepEqual(launchState(undefined, call, remoteRecord("a", "failed")), { phase: "failed", word: "Failed" });
  assert.deepEqual(launchState(undefined, call, remoteRecord("a", "cancelled")), { phase: "stopped", word: "Stopped" });
  assert.deepEqual(launchState(undefined, call, remoteRecord("a", "pending")), { phase: "working", word: "Starting" });
  assert.deepEqual(launchState(undefined, call, remoteRecord("a", "running", { stop_requested: true })), { phase: "working", word: "Stopping" });
  // A stale copy of the child that still works loses to a record that ended; a copy that ended wins over a running record.
  assert.deepEqual(launchState(delegated("t-a", "running"), call, remoteRecord("a", "completed")), { phase: "done", word: "Done" });
  assert.deepEqual(launchState(delegated("t-a", "completed"), call, remoteRecord("a", "running")), { phase: "done", word: "Done" });
  // A copy taken after the end is a child a message woke again: the record does not follow that, the copy wins. Only
  // when both times are the child's machine's (the record's end came from following it to its end).
  const followed = remoteRecord("a", "completed", { completed_at_from_target: true });
  const woken = delegated("t-a", "running", {}, { updated_at: iso(120) });
  assert.deepEqual(launchState(woken, call, followed), { phase: "working", word: "Working" });
  const stale = delegated("t-a", "running", {}, { updated_at: iso(30) });
  assert.deepEqual(launchState(stale, call, followed), { phase: "done", word: "Done" });
});

// The delegating machine records some ends by its own clock (cancelled before delivery, unreachable, a failed
// delivery): comparing that with the other machine's times would let a clock skew keep a stale copy "Working".
test("a record ended by the delegating machine's own clock wins over a copy that still works, whatever its time", () => {
  const call = { complete: true, isError: false };
  const later = delegated("t-a", "running", {}, { updated_at: iso(120) });
  for (const state of ["cancelled", "unreachable", "failed"]) {
    const ended = remoteRecord("a", state, { info: { ...remoteRecord("a", state).info, completed_at: iso(60) } });
    assert.equal(remoteRecordWins(later, ended), true, state);
    assert.notEqual(launchState(later, call, ended).phase, "working", state);
  }
  // Without any end time the settled record still wins, and a record still at work never does.
  assert.equal(remoteRecordWins(later, remoteRecord("a", "completed", { completed_at_from_target: true, info: { status: "completed" } })), true);
  assert.equal(remoteRecordWins(later, remoteRecord("a", "running")), false);
});

test("remote record states map onto the delegation tools' words", () => {
  assert.equal(remoteDelegationStatus("unreachable"), "failed");
  assert.equal(remoteDelegationStatus("pending"), "running");
  assert.deepEqual(remoteChildState({ state: "unreachable" }), { phase: "failed", word: "Unreachable" });
  assert.equal(isRemoteChildSettled({ state: "running" }), false);
  assert.equal(isRemoteChildSettled({ state: "cancelled" }), true);
});

test("records settle stale children and add rows for children the list lacks", () => {
  const parents = new Map([["root", { project_id: "p1", permission_mode: "full-access", cwd: "/w" }]]);
  const stale = delegated("t-a", "running", { task_id: "a" }, { status: "running" });
  const done = withRemoteRecords([stale], [remoteRecord("a", "completed"), remoteRecord("b", "running")], parents);
  assert.equal(done.length, 2);
  assert.deepEqual(childState(done[0]), { phase: "done", word: "Done" });
  assert.equal(done[0].status, "idle");
  assert.equal(done[0].delegation.result, "All done");
  assert.equal(done[1].id, "t-b");
  assert.equal(done[1].machineId, "satyamas");
  assert.equal(done[1].parent_thread_id, "root");
  assert.deepEqual(childState(done[1]), { phase: "working", word: "Working" });
  // Nothing recorded: the same array; a record whose parent is not listed adds nothing.
  const children = [stale];
  assert.equal(withRemoteRecords(children, [], parents), children);
  assert.equal(withRemoteRecords([], [remoteRecord("c", "running", { parent_thread_id: "elsewhere" })], parents).length, 0);
});
