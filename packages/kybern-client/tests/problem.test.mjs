import assert from "node:assert/strict";
import test from "node:test";
import { RpcCallError } from "../src/client.ts";
import { errorMessage, problemOf } from "../src/problem.ts";
import { classifyPrError, ghProblem, isGhSetup } from "../src/prReview.ts";

const sent = {
  kind: "tool_missing",
  subject: "gh",
  action: { id: "install_tool", label: "Install GitHub CLI", params: { tool: "gh" } },
  secondary: { id: "retry", label: "Check again" },
  detail: "run gh: program not found",
  retryable: true,
};

test("a daemon's problem is read from data.problem", () => {
  const error = new RpcCallError({ code: -32003, message: "The GitHub CLI (gh) isn't installed.", data: { problem: sent } });
  assert.deepEqual(problemOf(error), sent);
  assert.deepEqual(ghProblem(error), sent);
  assert.equal(errorMessage(error), "The GitHub CLI (gh) isn't installed.");
  assert.equal(isGhSetup(ghProblem(error)), true);
  // Unknown kinds and other data shapes are no problem Kybern knows.
  assert.equal(problemOf(new RpcCallError({ code: 1, message: "x", data: { problem: { kind: "new_kind" } } })), null);
  assert.equal(problemOf(new RpcCallError({ code: 1, message: "x", data: { kind: "move" } })), null);
  assert.equal(problemOf("plain"), null);
});

test("older daemons' messages still sort into a problem", () => {
  // What 0.21.1 said when gh wasn't on its PATH.
  const old = ghProblem(new RpcCallError({ code: -32602, message: "run gh" }));
  assert.equal(old.kind, "tool_missing");
  assert.equal(old.action?.id, "install_tool");
  assert.equal(old.detail, "run gh");
  assert.equal(ghProblem("RpcCallError: gh pr list: not logged in to any GitHub hosts").kind, "signed_out");
  assert.equal(ghProblem(new Error("gh pr view 3: HTTP 403: Resource not accessible")).kind, "permission");
  assert.equal(ghProblem(new Error("gh api x: HTTP 404: Not Found")).kind, "not_found");
  assert.deepEqual(ghProblem(new Error("gh pr list: boom")), { kind: "internal", detail: "gh pr list: boom", retryable: true });
  assert.equal(classifyPrError("This project has no GitHub remote. Add one, then try again."), "no-remote");
  assert.equal(classifyPrError("gh pr list: run gh fast"), "other");
  assert.equal(isGhSetup(ghProblem(new Error("boom"))), false);
});
