// What a machine's hardware and harnesses look like as short chips (Settings → Machines, ADE-86). Plain functions over
// the raw strings `machines.info` reports ("Intel(R) Xeon(R) Platinum 8255C CPU @ 2.50GHz"), so every client says them the same way.
import type { HarnessSummary, MachineInfo, ProviderKind } from "./types.ts";

export type OsKind = "macos" | "linux" | "windows";

const OS_KIND: Record<string, OsKind> = { macos: "macos", darwin: "macos", windows: "windows", linux: "linux" };

/** The OS family a machine reports (`macos`, `Darwin`, `Windows`, `linux`), or null for one we have no mark for. */
export function osKind(os: string | null | undefined): OsKind | null {
  return OS_KIND[(os ?? "").trim().toLowerCase()] ?? null;
}

const clean = (value: string) => value.replace(/\((?:R|TM|r|tm)\)|[®™]/g, "").replace(/\s+/g, " ").trim();

/** "Intel(R) Xeon(R) Platinum 8255C CPU @ 2.50GHz" → "Xeon Platinum 8255C"; "AMD EPYC 9354P 32-Core Processor" → "AMD EPYC 9354P". */
export function shortCpu(model: string | null | undefined): string | null {
  let name = clean(model ?? "");
  name = name
    .replace(/\s+with\s+Radeon.*$/i, "")
    .replace(/\s*@\s*[\d.]+\s*[GM]Hz.*$/i, "")
    .replace(/\s+\d+-Core Processor$/i, "")
    .replace(/\s+(?:CPU|Processor)$/i, "")
    .replace(/^\d+(?:st|nd|rd|th) Gen /i, "")
    .replace(/^Intel /i, "")
    .replace(/^AMD (?=Ryzen|Athlon|Threadripper)/i, "")
    .replace(/\s+(?:CPU|Processor)$/i, "")
    .trim();
  return name || null;
}

// Adapters a VM or a fallback driver shows: nothing a person picked, so nothing worth a chip.
const VIRTUAL_GPU = /cirrus|basic (?:display|render)|llvmpipe|softpipe|swiftshader|virtio|vmware|svga|qxl|bochs|standard vga|hyper-v|parallels|virtualbox|remote display|microsoft/i;

/** "NVIDIA GeForce RTX 3060 Laptop GPU" → "RTX 3060 Laptop"; "AMD Radeon(TM) Graphics" → "Radeon"; null for a virtual adapter. */
export function shortGpu(name: string | null | undefined): string | null {
  const full = clean(name ?? "");
  if (!full || VIRTUAL_GPU.test(full)) return null;
  const short = full
    .replace(/^(?:NVIDIA|AMD|Intel|ATI)\s+/i, "")
    .replace(/^GeForce\s+/i, "")
    .replace(/\s+Graphics\b/gi, "")
    .replace(/\s+GPU$/i, "")
    .trim();
  return short || full;
}

/** The GPUs worth showing: real ones, once each, and not the chip that is already the CPU ("Apple M1 · Apple M1"). */
export function visibleGpus(gpus: readonly string[] | undefined, cpuModel?: string | null): string[] {
  const cpu = shortCpu(cpuModel)?.toLowerCase();
  const seen = new Set<string>();
  const out: string[] = [];
  for (const raw of gpus ?? []) {
    const short = shortGpu(raw);
    if (!short) continue;
    const key = short.toLowerCase();
    if (key === cpu || (/^apple\b/.test(key) && !!cpu && /^apple\b/.test(cpu))) continue;
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(short);
  }
  return out;
}

/** "16 GB". */
export function memoryText(bytes: number | null | undefined): string | null {
  return bytes && bytes > 0 ? `${Math.max(1, Math.round(bytes / 2 ** 30))} GB` : null;
}

/** "16 cores", "1 core". */
export function coresText(cores: number | null | undefined): string | null {
  return cores && cores > 0 ? `${cores} ${cores === 1 ? "core" : "cores"}` : null;
}

export interface MachineSpecs {
  /** "Apple M1", null while unknown. */
  cpu: string | null;
  cores: string | null;
  memory: string | null;
  /** Real GPUs only, at most two. */
  gpus: string[];
}

export function machineSpecs(info: MachineInfo | false | null | undefined): MachineSpecs {
  if (!info) return { cpu: null, cores: null, memory: null, gpus: [] };
  return {
    cpu: shortCpu(info.cpu?.model),
    cores: coresText(info.cpu?.logical_cores),
    memory: memoryText(info.mem?.total_bytes),
    gpus: visibleGpus(info.gpu, info.cpu?.model).slice(0, 2),
  };
}

const HARNESS_NAME: Record<ProviderKind, string> = {
  "claude-code": "Claude",
  codex: "Codex",
  opencode: "OpenCode",
  pi: "pi",
  omp: "omp",
  cursor: "Cursor",
  antigravity: "Antigravity",
  "grok-build": "Grok Build",
};

/** "2.1.312 (Claude Code)", "codex-cli 0.165.0" and "omp/18.13.2" all say one number: the first dotted version in the text. */
export function shortVersion(version: string | null | undefined): string | null {
  const text = (version ?? "").trim();
  if (!text) return null;
  return /\d+(?:\.\d+)+(?:-[0-9A-Za-z.]+)?/.exec(text)?.[0] ?? text;
}

export interface HarnessChip {
  kind: ProviderKind;
  /** "Claude 2.1.312". */
  label: string;
  /** Installed, but nobody is signed in (only for harnesses with an account sign-in: Claude, Codex, Cursor, Antigravity). */
  signedOut: boolean;
}

/** Only the harnesses a machine has, each with its short name and number. */
export function harnessChips(harnesses: readonly HarnessSummary[] | undefined): HarnessChip[] {
  return (harnesses ?? [])
    .filter((harness) => harness.available)
    .map((harness) => {
      const name = HARNESS_NAME[harness.kind] ?? harness.kind;
      const version = shortVersion(harness.version);
      return { kind: harness.kind, label: version ? `${name} ${version}` : name, signedOut: SIGNS_IN.has(harness.kind) && harness.signed_in === false };
    });
}

/**
 * Harnesses with one account sign-in of their own. omp, OpenCode and pi run on whichever model
 * providers are configured in them, so "signed out" means nothing for them and is never shown.
 */
const SIGNS_IN: ReadonlySet<ProviderKind> = new Set<ProviderKind>(["claude-code", "codex", "cursor", "antigravity"]);
