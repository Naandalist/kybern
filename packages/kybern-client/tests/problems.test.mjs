import assert from "node:assert/strict";
import test from "node:test";
import { RpcCallError } from "../src/client.ts";
import { OfflineError } from "../src/machineClient.ts";
import { describeProblem, plainSentence } from "../src/problems.ts";

const rpc = (code, message, data) => new RpcCallError({ code, message, ...(data ? { data } : {}) });

/** Words that never belong in a headline (the spec's copy rules). */
const JARGON = /RpcCallError|invalid params|not found:|os error|exit code|EPERM|EACCES|[A-Za-z]:\\|\/Users\/|\/home\/|program not found|Error:/i;

// Real error texts, from the daemon (old ones without `data.problem` and current ones with it) and from the client.
// [what, error, context, expected kind, expected subject, expected primary action id]
const CASES = [
  ["gh missing, daemon 0.21.1", rpc(-32602, "run gh"), "load pull requests", "tool_missing", "gh", "install_tool"],
  ["gh missing, cause kept", rpc(-32602, "run gh: program not found"), "load pull requests", "tool_missing", "gh", "install_tool"],
  ["gh missing, current daemon", rpc(-32010, "The GitHub CLI (gh) isn't installed. Install it from cli.github.com, run gh auth login, and try again. (run gh: program not found)", {
    problem: { kind: "tool_missing", subject: "gh", action: { id: "install_tool", label: "Install GitHub CLI", params: { tool: "gh" } }, detail: "github.pr.list: run gh: program not found", retryable: true },
  }), "load pull requests", "tool_missing", "gh", "install_tool"],
  ["gh signed out", rpc(-32602, "gh pr list: To get started with GitHub CLI, please run:  gh auth login"), "load workflow runs", "signed_out", "gh", "sign_in"],
  ["git missing, old daemon", rpc(-32602, "run git"), "commit your changes", "tool_missing", "git", "install_tool"],
  ["git missing, Unix cause", rpc(-32602, "checkpoint: run git: No such file or directory (os error 2)"), "commit your changes", "tool_missing", "git", "install_tool"],
  ["git missing, current daemon", rpc(-32010, "Git isn't installed on this computer, or Kybern can't find it. Install it, then try again. (run git: program not found)", {
    problem: { kind: "tool_missing", subject: "git", detail: "run git: program not found", retryable: true },
  }), "commit your changes", "tool_missing", "git", "install_tool"],
  ["harness missing", rpc(-32602, "claude isn’t installed or isn’t on PATH"), "send your message", "tool_missing", "claude", "install_tool"],
  ["tailscale missing", rpc(-32603, "Tailscale is not installed on this machine"), "turn on the machine network", "tool_missing", "tailscale", "install_tool"],
  ["cursor needs node", rpc(-32602, "Cursor’s SDK installs with npm, which comes with Node.js. Install Node.js, then try again."), "set up Cursor", "tool_missing", "node", "install_tool"],
  ["npm ci failed", rpc(-32603, "npm ci failed: npm ERR! code ERESOLVE\nnpm ERR! ERESOLVE could not resolve"), "set up Cursor", "internal", undefined, "retry"],
  ["invalid params", rpc(-32602, "invalid params: missing field `thread_id` at line 1 column 2"), "rename the thread", "invalid_input", undefined, undefined],
  ["terminal gone", rpc(-32003, "not found: terminal"), "type in the terminal", "not_found", "terminal", "retry"],
  ["machine offline, daemon", rpc(-32013, "SatyaMas is offline."), "read notes", "offline", "SatyaMas", "retry"],
  ["machine offline, client", new OfflineError("m-1", false), "open the thread", "offline", undefined, "retry"],
  ["machine error code", { code: "unreachable", message: "Unable to connect to SatyaMas: Network request failed" }, "connect", "offline", undefined, "retry"],
  ["EPERM on Windows", new Error("EPERM: operation not permitted, symlink 'C:\\Program Files\\Kybern\\kybern.exe' -> 'C:\\Users\\Bagus\\bin\\kybern.exe'"), "add kybern to PATH", "permission", undefined, "retry"],
  ["Windows access denied", rpc(-32602, "write C:\\work\\a.txt: Access is denied. (os error 5)"), "save the file", "permission", undefined, "retry"],
  ["method missing", rpc(-32601, "method not found: notes.history"), "show note history", "update_needed", undefined, "update"],
  ["anything else", rpc(-32603, "threads.rename: save thread: database is locked"), "rename the thread", "internal", undefined, "retry"],
  ["a plain Error", new Error("boom"), "rename the thread", "internal", undefined, "retry"],
  ["a string", "RpcCallError: something odd", "rename the thread", "internal", undefined, "retry"],
];

for (const [what, error, action, kind, subject, actionId] of CASES) {
  test(`describeProblem: ${what}`, () => {
    const view = describeProblem(error, { action });
    assert.equal(view.kind, kind);
    assert.equal(view.subject, subject);
    assert.equal(view.action?.id, actionId);
    assert.ok(view.title.length > 0 && !/[.]$/.test(view.title), `a headline without a final period: ${view.title}`);
    assert.doesNotMatch(view.title, JARGON);
    assert.doesNotMatch(view.body, JARGON);
    assert.ok(view.body.length > 0);
    // The technical text is kept, verbatim, for Details.
    assert.ok(view.detail.length > 0);
  });
}

test("headlines name the fix and what it unlocks", () => {
  assert.equal(describeProblem(rpc(-32602, "run gh"), { action: "load pull requests", goal: "see pull requests" }).title, "Install GitHub CLI to see pull requests");
  assert.equal(describeProblem(rpc(-32602, "run git: program not found"), { action: "commit your changes" }).title, "Install Git to commit your changes");
  assert.equal(describeProblem(rpc(-32603, "database is locked"), { action: "rename the thread" }).title, "Couldn't rename the thread");
  assert.equal(describeProblem(rpc(-32013, "SatyaMas is offline."), { action: "read notes" }).title, "SatyaMas is offline");
  const remote = describeProblem(rpc(-32602, "run gh: program not found"), { action: "load pull requests", machine: "SatyaMas" });
  assert.match(remote.body, /isn't installed on SatyaMas/);
});

test("the detail is the daemon's full chain, else the message", () => {
  const sent = rpc(-32010, "Git isn't installed.", { problem: { kind: "tool_missing", subject: "git", detail: "checkpoint: run git: program not found" } });
  assert.equal(describeProblem(sent, { action: "save" }).detail, "checkpoint: run git: program not found");
  assert.equal(describeProblem(rpc(-32602, "invalid params: bad"), { action: "save" }).detail, "invalid params: bad");
});

test("the daemon's title and hint are the fallback only where the client has no words", () => {
  const unknownTool = rpc(-32010, "x", { problem: { kind: "tool_missing", title: "Install the helper", hint: "Kybern needs a helper program. Install it, then try again." } });
  const view = describeProblem(unknownTool, { action: "start" });
  assert.equal(view.title, "Install the helper");
  assert.equal(view.body, "Kybern needs a helper program. Install it, then try again.");
  // Client copy wins for a subject it knows.
  const gh = rpc(-32010, "x", { problem: { kind: "tool_missing", subject: "gh", title: "Daemon words" } });
  assert.equal(describeProblem(gh, { action: "load pull requests" }).title, "Install GitHub CLI to load pull requests");
});

test("a plain daemon sentence becomes the body; technical text never does", () => {
  assert.equal(describeProblem(rpc(-32004, "This note changed on another device. Choose which version to keep."), { action: "save the note" }).body, "This note changed on another device. Choose which version to keep.");
  assert.equal(plainSentence("The file changed on disk since you opened it"), "The file changed on disk since you opened it.");
  for (const technical of ["invalid params: x", "RpcCallError: x", "not found: terminal", "run git: program not found", "C:\\Users\\a is not a directory", "read /Users/me/x: denied", "git worktree add failed: fatal: x", "line one\nline two"])
    assert.equal(plainSentence(technical), null, technical);
});

test("String(error) never prints the class name", () => {
  const error = rpc(-32602, "run gh");
  assert.equal(String(error), "run gh");
  assert.equal(`${error}`, "run gh");
  const sent = rpc(-32010, "x", { problem: { kind: "tool_missing", subject: "gh" } });
  assert.deepEqual(sent.problem, { kind: "tool_missing", subject: "gh" });
  assert.equal(rpc(-32602, "x").problem, undefined);
});
