import type { ProviderKind, ProviderLimits, ProviderUsage } from "@/protocol"

export function reportedPercent(value: number): number | null {
  return Number.isFinite(value) ? Math.min(100, Math.max(0, value)) : null
}

export function contextUsage(context?: ProviderUsage["context"]) {
  if (!context || !Number.isFinite(context.used_tokens) || !Number.isFinite(context.window_tokens) || context.used_tokens < 0 || context.window_tokens <= 0) return null
  return { used: context.used_tokens, window: context.window_tokens, percent: reportedPercent(context.used_tokens / context.window_tokens * 100)! }
}

// Friendly names for the raw limit kinds harnesses report (Claude passes some
// through verbatim, e.g. "seven_day_overage_included").
const LIMIT_NAME_LABELS: Record<string, string> = {
  five_hour: "5-hour",
  seven_day: "Weekly",
  seven_day_overage_included: "Weekly (with overage)",
}

export function limitLabel(limit: NonNullable<ProviderUsage["limits"]>[number], provider?: ProviderKind): string {
  // Match each vendor's own wording: Claude calls these "Current session" and
  // "This week"; Codex (and the generic case) use the window durations.
  const claude = provider === "claude-code"
  if (limit.window_minutes === 300) return claude ? "Current session" : "5-hour"
  if (limit.window_minutes === 10080) return claude ? "This week" : "Weekly"
  const name = limit.name?.trim()
  if (!name) return "Usage limit"
  if (LIMIT_NAME_LABELS[name]) return LIMIT_NAME_LABELS[name]
  // Humanize an unknown snake_case / camelCase kind into Title Case words.
  const words = name.replace(/[_-]+/g, " ").replace(/([a-z])([A-Z])/g, "$1 $2").trim()
  return words ? words.charAt(0).toUpperCase() + words.slice(1) : "Usage limit"
}

type Limit = NonNullable<ProviderUsage["limits"]>[number]

export const PROVIDER_NAMES: Record<string, string> = { "claude-code": "Claude Code", codex: "Codex", cursor: "Cursor", opencode: "OpenCode", pi: "pi", omp: "Oh My Pi", antigravity: "Antigravity", "grok-build": "Grok Build" }

/** Status color only near a limit: 25% left warns, 10% left is critical. */
export function limitTone(percent: number | null): "normal" | "warning" | "critical" {
  if (percent === null) return "normal"
  if (percent >= 90) return "critical"
  if (percent >= 75) return "warning"
  return "normal"
}

function duration(ms: number): string {
  const minutes = Math.max(1, Math.round(ms / 60_000))
  if (minutes < 60) return `${minutes}m`
  if (minutes < 24 * 60) return `${Math.floor(minutes / 60)}h ${minutes % 60}m`
  return `${Math.floor(minutes / 1440)}d ${Math.floor((minutes % 1440) / 60)}h`
}

export type LimitPace = {
  /** Share of the window still to come, 0–100: where even spending would leave the bar. */
  evenLeft: number
  /** Short status: "12% in reserve", "Runs out in 3h 10m", "Limit reached". */
  label: string
  /** True when, at this rate, the limit runs out before it resets. */
  short: boolean
}

/**
 * Compare usage with time: a window that is 40% through and 30% used is 10
 * points in reserve. When spending outruns the window, project when the
 * limit runs out at the current rate. Needs a window length and reset time,
 * and skips the first 5% of a window, where one turn swings the projection.
 */
export function limitPace(limit: Limit, now: number): LimitPace | null {
  const used = reportedPercent(limit.used_percent)
  if (used === null || !limit.window_minutes || limit.resets_at == null) return null
  const windowMs = limit.window_minutes * 60_000
  const remainingMs = limit.resets_at * 1000 - now
  if (remainingMs <= 0 || remainingMs > windowMs) return null
  const elapsed = 1 - remainingMs / windowMs
  if (elapsed < 0.05) return null
  const evenLeft = (1 - elapsed) * 100
  if (used >= 100) return { evenLeft, label: "Limit reached", short: true }
  const reserve = Math.round(elapsed * 100 - used)
  const msToFull = used > 0 ? (100 - used) / (used / (elapsed * windowMs)) : Number.POSITIVE_INFINITY
  if (msToFull < remainingMs) return { evenLeft, label: `Runs out in ${duration(msToFull)}`, short: true }
  return { evenLeft, label: reserve > 0 ? `${reserve}% in reserve` : "On pace", short: false }
}

/** A window that reset after its last reading: how much of it is used now is unknown. */
export function resetSinceReading(limit: Limit, now: number): boolean {
  return limit.resets_at != null && Number.isFinite(limit.resets_at) && limit.resets_at * 1000 <= now
}

/** Percent of a window used, or null when unknown: not reported, or reset since it was read. */
export function limitUsed(limit: Limit, now: number): number | null {
  return resetSinceReading(limit, now) ? null : reportedPercent(limit.used_percent)
}

/** "62% left", or why there is no number. */
export function limitLeftLabel(limit: Limit, now: number): string {
  const used = limitUsed(limit, now)
  if (used !== null) return `${Math.round(100 - used)}% left`
  return resetSinceReading(limit, now) ? "Not read since reset" : "Unavailable"
}

/** The limit closest to running out: it decides how much the account has left. */
export function bindingLimit(limits: readonly Limit[], now: number): { limit: Limit; used: number } | null {
  let best: { limit: Limit; used: number } | null = null
  for (const limit of limits) {
    const used = limitUsed(limit, now)
    if (used !== null && (!best || used > best.used)) best = { limit, used }
  }
  return best
}

/** "Resets in 2h 14m" within a day, otherwise the weekday or date and time. */
export function resetIn(seconds: number | null, now: number): string {
  if (seconds === null || !Number.isFinite(seconds)) return "Reset time unavailable"
  if (seconds * 1000 <= now) return "Reset since last update"
  const minutes = Math.round((seconds * 1000 - now) / 60_000)
  if (minutes <= 0) return "Resets now"
  if (minutes < 60) return `Resets in ${minutes}m`
  if (minutes < 24 * 60) return `Resets in ${Math.floor(minutes / 60)}h ${minutes % 60}m`
  const date = new Date(seconds * 1000)
  if (!Number.isFinite(date.getTime())) return "Reset time unavailable"
  const options: Intl.DateTimeFormatOptions = minutes < 6 * 24 * 60
    ? { weekday: "short", hour: "numeric", minute: "2-digit" }
    : { month: "short", day: "numeric" }
  return `Resets ${date.toLocaleString(undefined, options)}`
}

/** Age of a provider's values: "just now", "4 min ago", "3 h ago". */
export function updatedAgo(updatedAt: string | undefined, now: number): string | null {
  if (!updatedAt) return null
  const time = Date.parse(updatedAt)
  if (!Number.isFinite(time)) return null
  const minutes = Math.floor((now - time) / 60_000)
  if (minutes < 1) return "just now"
  if (minutes < 60) return `${minutes} min ago`
  const hours = Math.floor(minutes / 60)
  if (hours < 48) return `${hours} h ago`
  return `${Math.floor(hours / 24)} days ago`
}

/** Values older than this are shown as possibly out of date. */
export const LIMITS_STALE_MS = 10 * 60_000

/**
 * Values that may be out of date: from the last daemon run, a failed or limited read, or
 * simply old. Stale rings stay on screen, dimmed; a provider with values is never hidden.
 */
export function limitsStale(entry: Pick<ProviderLimits, "updated_at" | "source" | "stale">, now: number): boolean {
  const time = entry.updated_at ? Date.parse(entry.updated_at) : Number.NaN
  return !!entry.stale || entry.source === "stored" || !Number.isFinite(time) || now - time > LIMITS_STALE_MS
}

/** Why a provider's values are not current; null while reads succeed. */
export function staleReason(entry: Pick<ProviderLimits, "provider" | "stale" | "retry_at">, now: number): string | null {
  const name = PROVIDER_NAMES[entry.provider] ?? entry.provider
  switch (entry.stale) {
    case "login_refresh":
      return `Waiting for ${name} to refresh its sign-in.`
    case "throttled": {
      const at = entry.retry_at ? Date.parse(entry.retry_at) : Number.NaN
      if (!Number.isFinite(at) || at <= now) return "Couldn’t read the latest limits."
      return `Couldn’t read the latest limits. Next check at ${new Date(at).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" })}.`
    }
    case "unavailable":
      return "Couldn’t read the latest limits."
    case "from_last_run":
      return "From the last time Kybern ran."
    default:
      return null
  }
}

/** "Last updated 2 h ago. Waiting for Claude Code to refresh its sign-in.": the caption of a stale ring. */
export function staleCaption(entry: Pick<ProviderLimits, "provider" | "stale" | "retry_at" | "updated_at">, now: number): string | null {
  const reason = staleReason(entry, now)
  if (!reason) return null
  const ago = updatedAgo(entry.updated_at, now)
  return ago ? `Last updated ${ago}. ${reason}` : reason
}
