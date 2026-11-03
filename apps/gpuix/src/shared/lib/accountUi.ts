// Pure logic behind the account surfaces in the composer: the model picker's tab
// strip, its "follow defaults" line and the status glyph. No React and no `@/`
// imports, so node tests load it directly.

import type { AccountSummary, ProviderKind, ProviderLimits } from "../../../../../packages/kybern-client/src/types.ts"
import { hasSignIn } from "../../../../../packages/kybern-client/src/machineAgents.ts"
import { CLI_ACCOUNT_NAME, CLI_INSTANCE, isCliInstance, withoutSignIn } from "./accounts.ts"

/**
 * Limits of one account. Per-account entries carry `instance`; the default
 * account's are also the daemon's global entry, which covers an agent with a
 * single account and daemons that do not report per-account entries.
 */
export function limitsForAccount(
  account: Pick<AccountSummary, "provider" | "is_default">,
  entries: readonly ProviderLimits[],
  global: readonly ProviderLimits[],
): ProviderLimits | undefined {
  const { kind, instance } = account.provider
  return entries.find((entry) => entry.provider === kind && entry.instance === instance) ?? (account.is_default ? global.find((entry) => entry.provider === kind) : undefined)
}

// ── Model picker tabs ────────────────────────────────────────────────────────

export interface PickerAgent {
  kind: ProviderKind
  display_name: string
  available: boolean
}

export type PickerTab =
  | { type: "starred"; key: "starred"; selected: boolean; label: "Starred"; tooltip: "Starred models" }
  | {
      type: "account"
      key: string
      kind: ProviderKind
      instance: string
      /** "CLI account" for the implicit instance, otherwise the account's name. */
      name: string
      color: string | null
      selected: boolean
      /** The agent has more than one account, so each account gets its own tab. */
      multi: boolean
      /** The selected tab of a multi-account agent shows this text beside its mark. */
      label: string | null
      tooltip: string
      ariaLabel: string
      /** The account can't send until it is signed in again: selecting it opens the sign-in sheet. */
      needsSignIn: boolean
      /** Set when the machine the thread runs on has this agent signed out (the tab is dimmed, its tooltip says where). */
      signedOutOn?: string
      /** The agent isn't set up: selecting it opens its setup. */
      notSetUp: boolean
      /** Another agent while the thread can't change agent. */
      disabled: boolean
    }

/** The account list of one agent, CLI account first, then named accounts in list order. */
export function accountsOfKind(accounts: readonly AccountSummary[], kind: ProviderKind): AccountSummary[] {
  const ofKind = accounts.filter((account) => account.provider.kind === kind)
  return [...ofKind.filter((a) => isCliInstance(a.provider.instance)), ...ofKind.filter((a) => !isCliInstance(a.provider.instance))]
}

export function accountNeedsSignIn(account: Pick<AccountSummary, "provider" | "status">): boolean {
  if (withoutSignIn(account.provider.kind)) return false
  return !isCliInstance(account.provider.instance) && (account.status === "needs_sign_in" || account.status === "signed_out")
}

export function pickerTabs({
  agents,
  accounts,
  current,
  canPickProvider,
  starredView,
  machineName,
}: {
  agents: readonly PickerAgent[]
  accounts: readonly AccountSummary[]
  current: { kind: ProviderKind; instance: string }
  canPickProvider: boolean
  starredView: boolean
  /** The thread runs on another machine: list only the agents installed there, and mark one that is signed out there. */
  machineName?: string
}): PickerTab[] {
  const tabs: PickerTab[] = [{ type: "starred", key: "starred", selected: starredView, label: "Starred", tooltip: "Starred models" }]
  for (const agent of agents) {
    if (machineName && !agent.available) continue
    const own = accountsOfKind(accounts, agent.kind)
    const multi = own.length > 1
    const isCurrentKind = agent.kind === current.kind
    const currentInstance = current.instance || CLI_INSTANCE
    // A current instance that is no longer listed (removed account) falls back to the CLI account.
    const selectedInstance = own.some((a) => a.provider.instance === currentInstance) ? currentInstance : CLI_INSTANCE
    const entries = own.length
      ? own
      : [{ provider: { kind: agent.kind, instance: CLI_INSTANCE }, name: CLI_ACCOUNT_NAME, status: "unknown" } as Pick<AccountSummary, "provider" | "name" | "status"> & { color?: string }]
    for (const account of entries) {
      const instance = account.provider.instance
      const cli = isCliInstance(instance)
      const name = cli ? CLI_ACCOUNT_NAME : account.name
      // The CLI account is the machine's own login: on another machine, "signed out" there is worth saying.
      const signedOutThere = !!machineName && cli && hasSignIn(agent.kind) && (account.status === "signed_out" || account.status === "needs_sign_in")
      const needsSignIn = accountNeedsSignIn(account) || signedOutThere
      const selected = !starredView && isCurrentKind && (!multi || instance === selectedInstance)
      const base = multi ? `${agent.display_name} · ${name}` : agent.display_name
      const tooltip = !agent.available
        ? `${agent.display_name} isn’t installed`
        : !isCurrentKind && !canPickProvider
          ? `Start a new thread to use ${agent.display_name}`
          : signedOutThere
            ? `${base} · signed out on ${machineName}`
            : needsSignIn
              ? `${base} — needs sign-in`
              : base
      tabs.push({
        type: "account",
        key: `${agent.kind}:${instance}`,
        kind: agent.kind,
        instance,
        name,
        color: cli ? null : account.color ?? null,
        selected,
        multi,
        label: multi && selected ? (cli ? "CLI" : name) : null,
        tooltip,
        ariaLabel: !agent.available ? `${agent.display_name}, not set up` : signedOutThere ? `${base}, signed out on ${machineName}` : needsSignIn ? `${base}, needs sign-in` : base,
        needsSignIn,
        ...(signedOutThere ? { signedOutOn: machineName } : {}),
        notSetUp: !agent.available,
        disabled: !isCurrentKind && !canPickProvider,
      })
    }
  }
  return tabs
}

/** Next enabled tab for arrow keys in the tablist, wrapping at the ends. */
export function nextTabIndex(tabs: readonly { disabled?: boolean | undefined; [key: string]: unknown }[], from: number, step: 1 | -1): number {
  if (!tabs.length) return -1
  for (let i = 1; i <= tabs.length; i++) {
    const index = (((from + step * i) % tabs.length) + tabs.length) % tabs.length
    if (!tabs[index]!.disabled) return index
  }
  return from
}

export type FollowLine =
  | { state: "following"; name: string; color: string | null; text: string }
  | { state: "pinned"; name: string; color: string | null; text: string; action: "Follow defaults" }

/**
 * The quiet line under the strip: only for an agent with more than one account
 * and only on a thread. `followsDefaults` is undefined for a draft.
 */
export function followLine({
  accounts,
  current,
  followsDefaults,
}: {
  accounts: readonly AccountSummary[]
  current: { kind: ProviderKind; instance: string }
  followsDefaults: boolean | undefined
}): FollowLine | null {
  if (followsDefaults === undefined) return null
  const own = accountsOfKind(accounts, current.kind)
  if (own.length < 2) return null
  const account = own.find((a) => a.provider.instance === (current.instance || CLI_INSTANCE))
  const cli = !account || isCliInstance(account.provider.instance)
  const name = cli ? CLI_ACCOUNT_NAME : account.name
  const color = cli ? null : account.color ?? null
  return followsDefaults
    ? { state: "following", name, color, text: `Following defaults · ${name}` }
    : { state: "pinned", name, color, text: `This thread uses ${name}`, action: "Follow defaults" }
}

// ── Status glyph ─────────────────────────────────────────────────────────────

export type GlyphTone = "normal" | "warning" | "critical"

/**
 * Arc color. Context sets it (warning from 80%, critical from 95%). An account
 * with under 20% left also warns, but never reaches critical on its own, so
 * account limits win over context only for color.
 */
export function glyphTone(contextPercent: number | null, limitLeftPercent: number | null): GlyphTone {
  if (contextPercent !== null && contextPercent >= 95) return "critical"
  if (contextPercent !== null && contextPercent >= 80) return "warning"
  if (limitLeftPercent !== null && limitLeftPercent < 20) return "warning"
  return "normal"
}

export type CacheState = { state: "active" } | { state: "warm"; minutes: number } | { state: "cold" }

export function cacheState(running: boolean, remainingMinutes: number): CacheState {
  if (running) return { state: "active" }
  return remainingMinutes > 0 ? { state: "warm", minutes: remainingMinutes } : { state: "cold" }
}

/** Center dot opacity: absent when cold, half in the last minute of a warm cache. */
export function cacheDotOpacity(cache: CacheState | null): number {
  if (!cache || cache.state === "cold") return 0
  return cache.state === "warm" && cache.minutes <= 1 ? 0.5 : 1
}

export function cacheValue(cache: CacheState): string {
  return cache.state === "active" ? "Active" : cache.state === "warm" ? `Warm · ${cache.minutes} min left` : "Cold"
}

export function glyphAriaLabel({
  contextPercent,
  cache,
  account,
}: {
  contextPercent: number | null
  cache: CacheState | null
  account: { name: string; leftPercent: number; windowLabel: string } | null
}): string {
  const parts = [contextPercent === null ? "Context usage unavailable." : `Context ${Math.round(contextPercent)}% used.`]
  if (cache) {
    parts.push(
      cache.state === "active"
        ? "Prompt cache active."
        : cache.state === "warm"
          ? `Prompt cache warm, about ${cache.minutes} ${cache.minutes === 1 ? "minute" : "minutes"} left.`
          : "Prompt cache cold.",
    )
  }
  if (account) parts.push(`${account.name}: ${Math.round(account.leftPercent)}% of the ${account.windowLabel.toLowerCase()} limit left.`)
  return parts.join(" ")
}
