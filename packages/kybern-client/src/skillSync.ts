// Your skills on every machine, for clients (ADE-86; the daemon side is `network/skill_sync.rs`). Plain functions over
// `skills.network_view`: which machines have a skill, whether their copies match, what a copy would do on each target,
// and the `skills.copy` requests the UI sends. Nothing copies by itself; every request here starts from a user action.
import type {
  MachineId,
  ProviderKind,
  SkillCopyInfo,
  SkillCopyTarget,
  SkillHome,
  SkillNetworkEntry,
  SkillsCopyParams,
  SkillsCopyResult,
  SkillsMachineView,
  SkillsNetworkView,
} from "./types.ts";
import { SKILL_HOMES } from "./types.ts";

// ---- homes ----

export interface SkillHomeDetail {
  home: SkillHome;
  /** The usual folder; the machine's real path is in `SkillHomeInfo.path`. */
  path: string;
  /** The harnesses that load skills from it. */
  readers: ProviderKind[];
  /** The cross-agent folder most harnesses read. */
  universal: boolean;
}

export const SKILL_HOME_DETAILS: Record<SkillHome, SkillHomeDetail> = {
  agents: { home: "agents", path: "~/.agents/skills", readers: ["codex", "cursor", "opencode", "omp", "antigravity", "grok-build"], universal: true },
  claude: { home: "claude", path: "~/.claude/skills", readers: ["claude-code", "cursor", "opencode"], universal: false },
  codex: { home: "codex", path: "~/.codex/skills", readers: ["codex", "cursor"], universal: false },
  cursor: { home: "cursor", path: "~/.cursor/skills", readers: ["cursor"], universal: false },
  opencode: { home: "opencode", path: "~/.config/opencode/skills", readers: ["opencode"], universal: false },
  pi: { home: "pi", path: "~/.pi/agent/skills", readers: ["pi", "omp"], universal: false },
  omp: { home: "omp", path: "~/.omp/agent/skills", readers: ["omp"], universal: false },
};

const READER_NAME: Record<ProviderKind, string> = { "claude-code": "Claude Code", codex: "Codex", opencode: "OpenCode", pi: "pi", omp: "Oh My Pi", cursor: "Cursor", antigravity: "Antigravity", "grok-build": "Grok Build" };

const list = (items: readonly string[]) => new Intl.ListFormat("en", { style: "long", type: "conjunction" }).format(items);

/** "Claude Code, Cursor and OpenCode". */
export function readersText(home: SkillHome): string {
  return list(SKILL_HOME_DETAILS[home].readers.map((reader) => READER_NAME[reader]));
}

/** The harnesses that load a skill from any of these homes, deduplicated in a stable order. */
export function harnessesOf(homes: readonly SkillHome[]): ProviderKind[] {
  // Antigravity reads no skill folder Kybern syncs, so it is left out on purpose.
  const all: ProviderKind[] = ["claude-code", "codex", "cursor", "opencode", "pi", "omp"];
  return all.filter((reader) => homes.some((home) => SKILL_HOME_DETAILS[home].readers.includes(reader)));
}

// ---- a skill across machines ----

/** What a machine holds of one skill, compared with the version most machines have. */
export type SkillMachineState = "same" | "different" | "missing" | "unknown";

export interface SkillMachineCell {
  machine: SkillsMachineView;
  state: SkillMachineState;
  copies: SkillCopyInfo[];
  /** The version this machine holds (its first copy with a hash). */
  hash: string | null;
  /** A copy for this machine still waits in the outbox. */
  pending: boolean;
}

export type SkillSyncStatus = "in_sync" | "different" | "missing";

export interface SkillRowModel {
  name: string;
  title: string;
  description: string;
  status: SkillSyncStatus;
  /** "On all 3 machines", "Different on os-kdi", "Not on satyamas · Waiting for vps". */
  statusText: string;
  cells: SkillMachineCell[];
  entry: SkillNetworkEntry;
}

const reported = (machine: SkillsMachineView) => machine.state === "reported";

function hashOf(copies: readonly SkillCopyInfo[]): string | null {
  return copies.find((copy) => copy.hash)?.hash ?? null;
}

/** Each machine's state for one skill, in the view's machine order. */
export function cellsOf(view: SkillsNetworkView, entry: SkillNetworkEntry): SkillMachineCell[] {
  const main = entry.versions[0] ?? null;
  return view.machines.map((machine) => {
    const on = entry.machines.find((item) => item.machine_id === machine.id);
    const copies = on?.copies ?? [];
    const hash = hashOf(copies);
    const pending = (entry.pending ?? []).some((item) => item.machine_id === machine.id);
    const state: SkillMachineState = !reported(machine) ? "unknown" : !on ? "missing" : main && copies.some((copy) => copy.hash === main) ? "same" : main ? "different" : "same";
    return { machine, state, copies, hash, pending };
  });
}

export function rowOf(view: SkillsNetworkView, entry: SkillNetworkEntry): SkillRowModel {
  const cells = cellsOf(view, entry);
  const names = (state: SkillMachineState) => cells.filter((cell) => cell.state === state && !cell.pending).map((cell) => cell.machine.name);
  const different = names("different");
  const missing = names("missing");
  const waiting = cells.filter((cell) => cell.pending).map((cell) => cell.machine.name);
  const parts: string[] = [];
  if (different.length) parts.push(`Different on ${list(different)}`);
  if (missing.length) parts.push(`Not on ${list(missing)}`);
  if (waiting.length) parts.push(`Waiting for ${list(waiting)}`);
  const have = cells.filter((cell) => cell.state === "same" || cell.state === "different").length;
  const reporting = view.machines.filter(reported).length;
  if (!parts.length) parts.push(reporting <= 1 ? "On this machine" : have === reporting ? `On all ${reporting} machines` : `On ${have} of ${reporting} machines`);
  const status: SkillSyncStatus = different.length ? "different" : missing.length ? "missing" : "in_sync";
  return {
    name: entry.name,
    title: entry.display_name || entry.name,
    description: entry.description ?? "",
    status,
    statusText: parts.join(" · "),
    cells,
    entry,
  };
}

export type SkillFilter = "all" | "different" | "missing";

/** The rows that match a search and a filter, in the view's order (by name). */
export function skillRows(view: SkillsNetworkView, query = "", filter: SkillFilter = "all"): SkillRowModel[] {
  const needle = query.trim().toLowerCase();
  return view.skills
    .filter((entry) => !needle || entry.name.toLowerCase().includes(needle) || (entry.display_name ?? "").toLowerCase().includes(needle) || (entry.description ?? "").toLowerCase().includes(needle))
    .map((entry) => rowOf(view, entry))
    .filter((row) => filter === "all" || row.status === filter || (filter === "missing" && row.cells.some((cell) => cell.state === "missing")));
}

export function filterCounts(view: SkillsNetworkView): Record<SkillFilter, number> {
  const rows = skillRows(view);
  return {
    all: rows.length,
    different: rows.filter((row) => row.status === "different").length,
    missing: rows.filter((row) => row.cells.some((cell) => cell.state === "missing")).length,
  };
}

// ---- machines ----

export interface SkillMachineSummary {
  machine: SkillsMachineView;
  total: number;
  /** Skills other machines have and this one lacks, that can be copied from a machine whose copies agree. */
  missing: string[];
  different: string[];
  /** "48 skills · 3 missing · 1 different", or why it did not report. */
  detail: string;
}

export function machineSummaries(view: SkillsNetworkView): SkillMachineSummary[] {
  const rows = skillRows(view);
  return view.machines.map((machine) => {
    const cell = (row: SkillRowModel) => row.cells.find((item) => item.machine.id === machine.id);
    const total = rows.filter((row) => ["same", "different"].includes(cell(row)?.state ?? "")).length;
    const missing = rows.filter((row) => cell(row)?.state === "missing" && !cell(row)?.pending && !row.entry.differs).map((row) => row.name);
    const different = rows.filter((row) => cell(row)?.state === "different").map((row) => row.name);
    let detail: string;
    if (machine.state === "offline") detail = "Offline · copies wait for it";
    else if (machine.state === "unsupported") detail = "Needs a newer Kybern to share skills";
    else if (machine.state === "unreachable") detail = machine.detail ?? "Didn't answer";
    else {
      const parts = [total === 1 ? "1 skill" : `${total} skills`];
      if (missing.length) parts.push(`${missing.length} missing`);
      if (different.length) parts.push(`${different.length} different`);
      detail = parts.join(" · ");
    }
    return { machine, total, missing, different, detail };
  });
}

// ---- copying ----

export type CopyTargetState = "missing" | "same" | "different" | "offline" | "unsupported" | "unreachable" | "source";

export interface CopyTargetModel {
  machine: SkillsMachineView;
  state: CopyTargetState;
  /** The versions there that differ from the one sent: what "Replace" agrees to. */
  replaces: string[];
  /** "Not on it", "Has a different version", "Offline · gets it when it returns". */
  detail: string;
  /** Checked when the sheet opens. */
  preselected: boolean;
  selectable: boolean;
}

/** The machines that have the skill and could send it. */
export function sourcesOf(view: SkillsNetworkView, entry: SkillNetworkEntry): SkillsMachineView[] {
  return view.machines.filter((machine) => reported(machine) && entry.machines.some((on) => on.machine_id === machine.id && hashOf(on.copies)));
}

/** The source the sheet opens with: this machine when it has the most common version, else a machine that does. */
export function defaultSource(view: SkillsNetworkView, entry: SkillNetworkEntry): SkillsMachineView | null {
  const sources = sourcesOf(view, entry);
  const main = entry.versions[0];
  const holds = (machine: SkillsMachineView) => entry.machines.find((on) => on.machine_id === machine.id)?.copies.some((copy) => copy.hash === main);
  return sources.find((machine) => machine.is_self && holds(machine)) ?? sources.find(holds) ?? sources[0] ?? null;
}

/** The version a source sends. */
export function sourceHash(entry: SkillNetworkEntry, source: MachineId): string | null {
  return hashOf(entry.machines.find((on) => on.machine_id === source)?.copies ?? []);
}

/** The homes the skill is in on the source: where it goes by default. */
export function sourceHomes(entry: SkillNetworkEntry, source: MachineId): SkillHome[] {
  const copies = entry.machines.find((on) => on.machine_id === source)?.copies ?? [];
  const homes = SKILL_HOMES.filter((home) => copies.some((copy) => copy.home === home));
  return homes.length ? homes : ["agents"];
}

/** What the files a copy leaves out look like, for a hint: ".env, node_modules/". */
export function skippedOf(entry: SkillNetworkEntry, source: MachineId): string[] {
  const copies = entry.machines.find((on) => on.machine_id === source)?.copies ?? [];
  return [...new Set(copies.flatMap((copy) => copy.skipped ?? []))];
}

export function copyTargets(view: SkillsNetworkView, entry: SkillNetworkEntry, source: MachineId): CopyTargetModel[] {
  const sent = sourceHash(entry, source);
  return view.machines
    .filter((machine) => machine.id !== source)
    .map((machine) => {
      const copies = entry.machines.find((on) => on.machine_id === machine.id)?.copies ?? [];
      const hashes = [...new Set(copies.map((copy) => copy.hash).filter((hash): hash is string => !!hash))];
      const replaces = hashes.filter((hash) => hash !== sent);
      let state: CopyTargetState;
      if (machine.state === "offline") state = "offline";
      else if (machine.state === "unsupported") state = "unsupported";
      else if (machine.state === "unreachable") state = "unreachable";
      else if (!copies.length) state = "missing";
      else if (replaces.length) state = "different";
      else state = "same";
      const detail = {
        missing: "Doesn't have it",
        same: "Has this version",
        different: "Has a different version",
        offline: "Offline · gets it when it returns",
        unsupported: "Needs a newer Kybern",
        unreachable: machine.detail ?? "Didn't answer",
        source: "Sends it",
      }[state];
      return { machine, state, replaces, detail, preselected: state === "missing", selectable: state !== "unsupported" && state !== "unreachable" };
    });
}

/** The `skills.copy` request for the chosen machines and homes. A different version is replaced only when `replace` is on. */
export function copyRequest(entry: SkillNetworkEntry, source: SkillsMachineView, targets: readonly CopyTargetModel[], homes: readonly SkillHome[], replace: boolean): SkillsCopyParams {
  const to: SkillCopyTarget[] = targets.map((target) => ({
    machine: target.machine.id,
    homes: [...homes],
    ...(replace && target.replaces.length ? { replaces: [...target.replaces] } : {}),
  }));
  return { name: entry.name, ...(source.is_self ? {} : { from: source.id }), to };
}

/** "Copy all missing" for one machine: one request per skill, each from a machine whose copy is the common version. */
export function missingRequests(view: SkillsNetworkView, machineId: MachineId): SkillsCopyParams[] {
  const summary = machineSummaries(view).find((item) => item.machine.id === machineId);
  if (!summary) return [];
  return summary.missing.flatMap((name) => {
    const entry = view.skills.find((skill) => skill.name === name);
    const source = entry && defaultSource(view, entry);
    if (!entry || !source) return [];
    return [{ name, ...(source.is_self ? {} : { from: source.id }), to: [{ machine: machineId, homes: sourceHomes(entry, source.id) }] }];
  });
}

/** One line per machine after a copy: "Copied", "Queued · arrives when satyamas is reachable", or the reason. */
export function copyOutcomeText(outcome: SkillsCopyResult["targets"][number]): string {
  switch (outcome.state) {
    case "queued":
      return `Sending · arrives when ${outcome.name} is reachable`;
    case "done": {
      const replaced = (outcome.placed ?? []).filter((placed) => placed.action === "replaced").length;
      return replaced ? "Copied · the old version is kept in Kybern's data folder" : "Copied";
    }
    case "unsupported":
      return outcome.error ?? `${outcome.name} needs a newer Kybern`;
    case "failed":
      return outcome.error ?? "Not copied";
  }
}

/** "Copy to os-kdi", "Copy to 2 machines", "Replace on os-kdi", or "Copy and replace" when only some are replaced. */
export function copyButtonLabel(targets: readonly CopyTargetModel[], replace: boolean): string {
  if (!targets.length) return "Copy";
  const name = targets.length === 1 ? targets[0]!.machine.name : `${targets.length} machines`;
  if (!replace) return `Copy to ${name}`;
  return targets.every((target) => target.state === "different") ? `Replace on ${name}` : "Copy and replace";
}
