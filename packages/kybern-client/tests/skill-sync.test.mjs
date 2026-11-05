import assert from "node:assert/strict";
import test from "node:test";
import {
  copyButtonLabel,
  copyRequest,
  copyTargets,
  defaultSource,
  filterCounts,
  harnessesOf,
  machineSummaries,
  missingRequests,
  readersText,
  skillRows,
  sourceHomes,
} from "../src/skillSync.ts";

const machines = [
  { id: "mac", name: "mac", is_self: true, state: "reported" },
  { id: "kdi", name: "os-kdi", is_self: false, state: "reported" },
  { id: "win", name: "satyamas", is_self: false, state: "reported" },
  { id: "vps", name: "vps", is_self: false, state: "offline" },
];
const copy = (home, hash, extra = {}) => ({ home, path: `/x/${home}`, hash, size: 10, files: 1, ...extra });
const view = {
  machines,
  skills: [
    { name: "better-ui", description: "Polish.", machines: [{ machine_id: "mac", copies: [copy("claude", "h1")] }, { machine_id: "kdi", copies: [copy("claude", "h1")] }, { machine_id: "win", copies: [copy("agents", "h1")] }], versions: ["h1"], differs: false },
    { name: "review", machines: [{ machine_id: "mac", copies: [copy("agents", "r1"), copy("claude", "r1", { linked_to: "agents", is_link: true })] }, { machine_id: "kdi", copies: [copy("claude", "r2")] }, { machine_id: "win", copies: [copy("agents", "r1")] }], versions: ["r1", "r2"], differs: true },
    { name: "deploy", machines: [{ machine_id: "kdi", copies: [copy("codex", "d1")] }], versions: ["d1"], differs: false, pending: [{ machine_id: "win", outbox_id: "o", from: "mac" }] },
  ],
};

test("a row says where a skill differs, where it is missing and what still waits, and an offline machine never counts", () => {
  const rows = skillRows(view);
  assert.deepEqual(rows.map((row) => [row.name, row.status, row.statusText]), [
    ["better-ui", "in_sync", "On all 3 machines"],
    ["review", "different", "Different on os-kdi"],
    ["deploy", "missing", "Not on mac · Waiting for satyamas"],
  ]);
  assert.deepEqual(rows[1].cells.map((cell) => cell.state), ["same", "different", "same", "unknown"]);
  assert.deepEqual(filterCounts(view), { all: 3, different: 1, missing: 1 });
  assert.deepEqual(skillRows(view, "polish").map((row) => row.name), ["better-ui"]);
  assert.deepEqual(skillRows(view, "", "different").map((row) => row.name), ["review"]);
});

test("machine summaries count missing skills that can be copied, and an offline machine says copies wait", () => {
  const summaries = machineSummaries(view);
  assert.equal(summaries[0].detail, "2 skills · 1 missing");
  assert.deepEqual(summaries[0].missing, ["deploy"]);
  assert.equal(summaries[1].detail, "3 skills · 1 different");
  assert.deepEqual(summaries[2].missing, [], "a copy already on its way is not missing");
  assert.equal(summaries[3].detail, "Offline · copies wait for it");
  assert.deepEqual(missingRequests(view, "mac"), [{ name: "deploy", from: "kdi", to: [{ machine: "mac", homes: ["codex"] }] }]);
});

test("copy targets say what each machine has, and Replace sends only the versions the user saw", () => {
  const review = view.skills[1];
  const source = defaultSource(view, review);
  assert.equal(source.id, "mac", "this machine holds the common version");
  assert.deepEqual(sourceHomes(review, "mac"), ["agents", "claude"], "a linked home counts as its own");
  const targets = copyTargets(view, review, "mac");
  assert.deepEqual(targets.map((target) => [target.machine.id, target.state, target.preselected]), [
    ["kdi", "different", false],
    ["win", "same", false],
    ["vps", "offline", false],
  ]);
  const kdi = targets.filter((target) => target.machine.id === "kdi");
  assert.deepEqual(copyRequest(review, source, kdi, ["claude"], false), { name: "review", to: [{ machine: "kdi", homes: ["claude"] }] });
  assert.deepEqual(copyRequest(review, source, kdi, ["claude"], true), { name: "review", to: [{ machine: "kdi", homes: ["claude"], replaces: ["r2"] }] });
  assert.equal(copyButtonLabel(kdi, true), "Replace on os-kdi");
  assert.equal(copyButtonLabel(targets, false), "Copy to 3 machines");
  assert.equal(copyButtonLabel(targets, true), "Copy and replace", "only os-kdi is replaced");
  const deploy = copyRequest(view.skills[2], machines[1], [], ["codex"], false);
  assert.equal(deploy.from, "kdi", "another machine sends it");
});

test("homes name the harnesses that read them", () => {
  assert.equal(readersText("agents"), "Codex, Cursor, OpenCode, Oh My Pi, Antigravity, and Grok Build");
  assert.deepEqual(harnessesOf(["claude", "pi"]), ["claude-code", "cursor", "opencode", "pi", "omp"]);
});
