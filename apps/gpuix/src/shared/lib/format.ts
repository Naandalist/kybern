import { imageSafeValue } from "../../../../../packages/kybern-client/src/responseImages.ts"
import type { JsonValue, PermissionMode, ProviderKind, ThreadStatus } from "@/protocol"

export { toolLine } from "../../../../../packages/kybern-client/src/toolActivity.ts"

export function relativeTime(iso: string, now = Date.now()): string {
  const t = Date.parse(iso)
  if (Number.isNaN(t)) return ""
  const s = Math.max(0, Math.round((now - t) / 1000))
  if (s < 45) return "now"
  const m = Math.round(s / 60)
  if (m < 60) return `${m}m`
  const h = Math.round(m / 60)
  if (h < 24) return `${h}h`
  const d = Math.round(h / 24)
  if (d < 7) return `${d}d`
  const w = Math.round(d / 7)
  if (w < 5) return `${w}w`
  return new Date(t).toLocaleDateString(undefined, { month: "short", day: "numeric" })
}

export function clockTime(iso: string): string {
  const t = Date.parse(iso)
  if (Number.isNaN(t)) return ""
  return new Date(t).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" })
}

/** Local calendar identity for transcript sectioning. */
export function calendarDateKey(iso: string): string {
  const date = new Date(iso)
  if (Number.isNaN(date.getTime())) return ""
  return `${date.getFullYear()}-${date.getMonth() + 1}-${date.getDate()}`
}

/** Exact local date with a relative cue for the two dates people scan most. */
export function calendarDateLabel(iso: string, now = new Date()): string {
  const date = new Date(iso)
  if (Number.isNaN(date.getTime())) return ""
  const key = calendarDateKey(iso)
  const today = calendarDateKey(now.toISOString())
  const yesterday = new Date(now.getFullYear(), now.getMonth(), now.getDate() - 1)
  const relative = key === today ? "Today" : key === calendarDateKey(yesterday.toISOString()) ? "Yesterday" : null
  const exact = date.toLocaleDateString(undefined, { weekday: "long", month: "short", day: "numeric", year: "numeric" })
  return relative ? `${relative} · ${exact}` : exact
}

export function dayBucket(iso: string, now = Date.now()): "Today" | "Yesterday" | "This week" | "Earlier" {
  const t = new Date(iso)
  const n = new Date(now)
  const startOfDay = (d: Date) => new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime()
  const diffDays = Math.round((startOfDay(n) - startOfDay(t)) / 86_400_000)
  if (diffDays <= 0) return "Today"
  if (diffDays === 1) return "Yesterday"
  if (diffDays < 7) return "This week"
  return "Earlier"
}

export function duration(ms: number): string {
  if (ms < 1000) return `${Math.max(1, Math.round(ms / 100) / 10)}s`
  const s = Math.round(ms / 1000)
  if (s < 60) return `${s}s`
  const m = Math.floor(s / 60)
  const r = s % 60
  if (m < 60) return r ? `${m}m ${r}s` : `${m}m`
  const h = Math.floor(m / 60)
  return `${h}h ${m % 60}m`
}

export function elapsedSince(iso: string, now = Date.now()): number {
  const t = Date.parse(iso)
  return Number.isNaN(t) ? 0 : Math.max(0, now - t)
}

export function tokens(n: number): string {
  if (n < 1000) return String(n)
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}k`
  return `${(n / 1_000_000).toFixed(1)}M`
}

export function usd(n: number): string {
  return n < 0.01 ? "<$0.01" : `$${n.toFixed(2)}`
}

export function plural(n: number, one: string, many = `${one}s`): string {
  return `${n} ${n === 1 ? one : many}`
}

export function shortenPath(p: string, keep = 3): string {
  const parts = p.split("/").filter(Boolean)
  return parts.length <= keep ? p : `…/${parts.slice(-keep).join("/")}`
}

export function basename(p: string): string {
  const parts = p.split("/").filter(Boolean)
  return parts[parts.length - 1] ?? p
}

export const PERMISSION_LABEL: Record<PermissionMode, string> = {
  supervised: "Supervised",
  "accept-edits": "Accept edits",
  auto: "Auto",
  "full-access": "Full access",
}

export const PERMISSION_HINT: Record<PermissionMode, string> = {
  supervised: "Asks before every edit, command, and network call.",
  "accept-edits": "Edits files freely. Asks before commands and network calls.",
  auto: "The agent decides and only asks when unsure.",
  "full-access": "Never asks. Use in a sandbox or a worktree.",
}

export const PROVIDER_LABEL: Record<ProviderKind, string> = {
  "claude-code": "Claude Code",
  codex: "Codex",
  opencode: "OpenCode",
  pi: "pi",
  omp: "Oh My Pi",
  cursor: "Cursor",
  antigravity: "Antigravity",
  "grok-build": "Grok Build",
}

/** Availability check for collapsed rows; never serialize a full result. */
export function hasOutputText(output: JsonValue | null, stream: string): boolean {
  if (typeof output === "string") return output.trim().length > 0
  if (output && typeof output === "object") {
    for (const key of ["output", "stdout", "content", "text", "result"]) {
      const value = (output as Record<string, JsonValue>)[key]
      if (typeof value === "string") return (imageSafeValue.call(output, key, value) as string).trim().length > 0
    }
    return true // JSON arrays/objects always have visible delimiters.
  }
  return stream.trim().length > 0
}

export function outputText(output: JsonValue | null, stream: string): string {
  if (typeof output === "string") return output
  if (output && typeof output === "object") {
    const o = output as Record<string, unknown>
    for (const k of ["output", "stdout", "content", "text", "result"]) {
      const v = o[k]
      if (typeof v === "string") return imageSafeValue.call(output, k, v) as string
    }
    return JSON.stringify(output, imageSafeValue, 2)
  }
  return stream
}

export const isMac = /Mac/.test(navigator.userAgent)
export const mod = isMac ? "⌘" : "Ctrl"

export const STATUS_LABEL: Record<ThreadStatus, string> = {
  idle: "",
  running: "Working",
  "awaiting-approval": "Needs approval",
  failed: "Failed",
  archived: "Archived",
}

const EFFORT_LABEL: Record<string, string> = { xhigh: "X-High" }

/** Effort ids are lowercase wire values (`xhigh`); show them as words. */
export function formatEffort(effort: string): string {
  return EFFORT_LABEL[effort] ?? effort.charAt(0).toUpperCase() + effort.slice(1)
}

export { truncateMiddle } from "@/lib/truncate"
