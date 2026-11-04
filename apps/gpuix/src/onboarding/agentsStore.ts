// What the agents scene knows: each agent's detected state (the daemon's `providers.list` plus its accounts, Cursor's own SDK
// status), what the user is doing to it right now (signing in, installing), which one starts new chats, the open popover and the
// terminal sheet. Module state, so the scene can come and go; one `useAgents()` folds it into the six tiles.
import { homedir } from 'node:os'
import { useMemo } from 'react'
import { activeEnvironment } from '../shell/environment.ts'
import { ACCOUNTS_LOGIN_CHANGED_NOTIFICATION, type AccountLogin, type AccountSummary, type CursorSetupStatus, type ProviderKind, type ProviderStatus, type TerminalInfo } from '../../../../packages/kybern-client/src/types.ts'
import { showDialog } from '../dialogs/store.ts'
import { loadProviders } from '../nav/live.ts'
import { useNav } from '../nav/store.ts'
import { openExternal } from '../settings/external.ts'
import { latestSettings, refreshSettings, updateSettings } from '../settings/store.ts'
import { getClient } from '../state.ts'
import { refreshAccounts, useAccounts } from '../usage/accounts.ts'
import { AGENT_NAMES, AGENT_ORDER, agentCommand, applyOverride, deriveAgent, pickDefault, type AgentOverride, type AgentState } from './model.ts'
import { announce, defaultAgentAnnouncement } from './announce.ts'
import { createStore } from './store.ts'
import { openTypedTerminal } from './typedTerminal.ts'

export type SheetMode = 'install' | 'login' | 'update'
export interface Sheet {
  kind: ProviderKind
  mode: SheetMode
  title: string
  command: string
  info: TerminalInfo | null
  /** The program in the terminal ended. */
  exited: boolean
  error: string | null
}

interface AgentsState {
  userDefault: ProviderKind | null
  override: Partial<Record<ProviderKind, AgentOverride>>
  /** Detection has landed for this agent (a short stagger after the data, so results arrive one after another). */
  revealed: Partial<Record<ProviderKind, boolean>>
  cursor: CursorSetupStatus | null
  popover: ProviderKind | null
  sheet: Sheet | null
  /** Bumped when an agent turns ready after the first reveal: the tile swells and the hub answers. */
  pulse: Partial<Record<ProviderKind, number>>
  connects: number
}
export const agents = createStore<AgentsState>({ userDefault: null, override: {}, revealed: {}, cursor: null, popover: null, sheet: null, pulse: {}, connects: 0 })
const setOverride = (kind: ProviderKind, value: AgentOverride) => agents.set((s) => ({ override: { ...s.override, [kind]: value } }))

// ─── Detection ────────────────────────────────────────────────────────────

let detected = false
const timers = new Set<ReturnType<typeof setTimeout>>()
const later = (ms: number, fn: () => void) => { const id = setTimeout(() => { timers.delete(id); fn() }, ms); timers.add(id) }

async function readCursor() {
  try { agents.set({ cursor: await getClient()!.call('cursor.status', {}) }) } catch { /* a daemon without Cursor setup: the tile reads from providers */ }
}

/** Probe every agent once per run (the scene may be left and revisited). The tiles settle 190 ms apart once the answers are in. */
export function startDetection(force = false) {
  if (detected && !force) return
  detected = true
  const reads = Promise.all([loadProviders(true).catch(() => []), refreshAccounts({ probe: true }), readCursor()])
  const floor = new Promise<void>((resolve) => later(900, resolve))
  void Promise.all([reads, floor]).then(() => {
    AGENT_ORDER.forEach((kind, i) => later(i * 190, () => agents.set((s) => ({ revealed: { ...s.revealed, [kind]: true } }))))
  })
}

/** Check one agent again (a click on "Check sign-in", the end of a terminal, a finished sign-in): it shows Checking… until the answer lands. */
export async function recheck(kind: ProviderKind) {
  agents.set((s) => ({ revealed: { ...s.revealed, [kind]: false } }))
  const started = Date.now()
  await Promise.all([loadProviders(true).catch(() => []), refreshAccounts({ probe: true }), kind === 'cursor' ? readCursor() : Promise.resolve()])
  later(Math.max(0, 700 - (Date.now() - started)), () => agents.set((s) => ({ revealed: { ...s.revealed, [kind]: true } })))
}

export function resetAgents() {
  for (const id of timers) clearTimeout(id)
  timers.clear()
  detected = false
  stopCursorPoll()
  stopWatching()
  opened = ''
  agents.reset({ userDefault: null, override: {}, revealed: {}, cursor: null, popover: null, sheet: null, pulse: {}, connects: 0 })
}

// ─── The tiles ────────────────────────────────────────────────────────────

export interface Agent {
  kind: ProviderKind
  name: string
  state: AgentState
  /** What the daemon said when it is not available (shown in the popover). */
  reason: string | null
  version: string | null
  email: string | null
}

/** The derived state of one agent from the three sources. Pure given its inputs. */
export function agentFor(kind: ProviderKind, providers: readonly ProviderStatus[], accounts: readonly AccountSummary[], cursor: CursorSetupStatus | null, loaded: boolean): Pick<Agent, 'state' | 'reason' | 'version' | 'email'> {
  const status = providers.find((p) => p.kind === kind)
  const mine = accounts.filter((a) => a.provider.kind === kind)
  let state = deriveAgent(status, mine, loaded)
  if (kind === 'cursor' && status) {
    // Cursor's sign-in lives in its own SDK status, not in the account list.
    if (status.available && cursor) state = cursor.account === 'signed_in' || cursor.account === 'api_key' ? 'ready' : cursor.account === 'signed_out' ? 'signin' : 'unknown'
    else if (!status.available) state = cursor?.problem ? 'update' : 'missing'
  }
  const signed = mine.find((a) => a.status === 'signed_in')
  const reason = status?.unavailable_reason ?? (kind === 'cursor' ? (cursor?.problem ?? cursor?.error ?? null) : null)
  const email = signed?.identity?.email ?? (kind === 'cursor' ? (cursor?.email ?? null) : null)
  return { state, reason, version: status?.version ?? null, email }
}

export function useAgents() {
  const providers = useNav((s) => s.providers)
  const loaded = useNav((s) => s.providersLoaded)
  const accounts = useAccounts()
  const override = agents.use((s) => s.override)
  const revealed = agents.use((s) => s.revealed)
  const cursor = agents.use((s) => s.cursor)
  const userDefault = agents.use((s) => s.userDefault)
  return useMemo(() => {
    const list: Agent[] = AGENT_ORDER.map((kind) => {
      const raw = agentFor(kind, providers, accounts, cursor, loaded)
      const own = override[kind] ?? null
      const shown: AgentState = own === 'waiting' || own === 'installing' ? own : revealed[kind] ? applyOverride(raw.state, own) : 'checking'
      return { kind, name: AGENT_NAMES[kind], ...raw, state: shown }
    })
    const ready = list.filter((a) => a.state === 'ready').map((a) => a.kind)
    return { list, ready, default: pickDefault(ready, userDefault) }
  }, [providers, loaded, accounts, cursor, override, revealed, userDefault])
}

export const useAgentFor = (kind: ProviderKind) => useAgents().list.find((a) => a.kind === kind)!

/** The user picked this agent for new chats ("Start new chats with it"); it stays put from now on. */
export const setDefaultAgent = (kind: ProviderKind) => { agents.set({ userDefault: kind }); announce(defaultAgentAnnouncement(kind)) }

/**
 * Leaving the step writes the default agent (every ready agent stays usable). `updateSettings` reports a failure through the
 * settings store instead of throwing, and skips a write while another is saving, so the answer is whether the value stuck.
 */
export async function saveDefaultAgent(kind: ProviderKind | null): Promise<boolean> {
  if (!kind) return true
  for (let attempt = 0; attempt < 4; attempt++) {
    if (!latestSettings()) await refreshSettings()
    if (latestSettings()?.default_provider === kind) return true
    await updateSettings({ default_provider: kind })
    if (latestSettings()?.default_provider === kind) return true
    await new Promise((resolve) => setTimeout(resolve, 150))
  }
  return false
}

// ─── Browser sign-in (the existing Add account sheet) ─────────────────────

let watching: (() => void) | null = null
/** Mirror the Add account sheet's sign-in on the tile: "Waiting for browser…" while it runs, a re-check when it finishes. */
export function watchLogins() {
  if (watching) return
  const client = getClient()
  if (!client) return
  const off = client.onNotification(ACCOUNTS_LOGIN_CHANGED_NOTIFICATION, (params) => {
    const login = params as AccountLogin
    if (!login?.kind) return
    if (login.phase === 'starting' || login.phase === 'waiting' || login.phase === 'verifying') setOverride(login.kind, 'waiting')
    else {
      setOverride(login.kind, null)
      if (login.phase === 'signed_in') void recheck(login.kind)
    }
  })
  watching = () => { off(); watching = null }
}
export function stopWatching() { watching?.() }

export function signInWithBrowser(kind: ProviderKind) {
  agents.set({ popover: null })
  if (kind === 'cursor') { void cursorSetup('sign_in'); return }
  showDialog({ kind: 'account', provider: kind })
}

// ─── Cursor ───────────────────────────────────────────────────────────────

let cursorPoll: ReturnType<typeof setTimeout> | undefined
const stopCursorPoll = () => clearTimeout(cursorPoll)
let opened = ''

/** Cursor installs and signs in with one click each (`cursor.setup`); the tile follows the daemon's own progress. */
export async function cursorSetup(action: 'install' | 'sign_in' | 'cancel_sign_in') {
  const client = getClient()
  if (!client) return
  agents.set({ popover: null })
  setOverride('cursor', action === 'install' ? 'installing' : action === 'sign_in' ? 'waiting' : null)
  try {
    const next = await client.call('cursor.setup', { action })
    agents.set({ cursor: next })
    if (next.signing_in && next.login_url && opened !== next.login_url) { opened = next.login_url; void openExternal(next.login_url).catch(() => {}) }
    followCursor()
  } catch {
    setOverride('cursor', null)
    void recheck('cursor')
  }
}
function followCursor() {
  stopCursorPoll()
  cursorPoll = setTimeout(async () => {
    const client = getClient()
    if (!client) return
    try {
      const next = await client.call('cursor.status', {})
      agents.set({ cursor: next })
      if (next.installing || next.signing_in) { followCursor(); return }
    } catch { /* fall through to a re-check */ }
    setOverride('cursor', null)
    void recheck('cursor')
  }, 1500)
}

// ─── The terminal sheet ───────────────────────────────────────────────────

/** Open the sheet: a shell with the command typed and never run. Return runs it; nothing runs before. */
export async function openSheet(kind: ProviderKind, mode: SheetMode) {
  const client = getClient()
  // A remote machine's own OS is not known here; the terminal there is a POSIX shell in practice.
  const command = agentCommand(kind, mode === 'login' ? 'login' : mode, activeEnvironment().local ? process.platform : 'linux')
  if (!client || !command) return
  const name = AGENT_NAMES[kind]
  agents.set({ popover: null, sheet: { kind, mode, title: mode === 'login' ? `Sign in to ${name}` : mode === 'update' ? `Update ${name}` : `Install ${name}`, command, info: null, exited: false, error: null } })
  setOverride(kind, 'installing')
  try {
    const wanted = (info: TerminalInfo) => { const live = agents.get().sheet; return live?.kind === kind && (!live.info || live.info.id === info.id) }
    const info = await openTypedTerminal(client, command, { cwd: activeEnvironment().local ? homedir() : '/', wanted, later })
    if (info) agents.set((s) => ({ sheet: s.sheet ? { ...s.sheet, info } : s.sheet }))
  } catch (error) {
    agents.set((s) => ({ sheet: s.sheet ? { ...s.sheet, error: error instanceof Error ? error.message : String(error) } : s.sheet }))
  }
}

export const sheetExited = () => agents.set((s) => ({ sheet: s.sheet ? { ...s.sheet, exited: true } : s.sheet }))

/** Close the sheet and check that agent again, which shows Checking… and lands on the new state. */
export function closeSheet() {
  const sheet = agents.get().sheet
  if (!sheet) return
  agents.set({ sheet: null })
  setOverride(sheet.kind, null)
  if (sheet.info) void getClient()?.call('terminals.close', { terminal_id: sheet.info.id }).catch(() => {})
  void recheck(sheet.kind)
}

/** "It already works": count an agent whose sign-in Kybern cannot tell as ready. */
export function assumeReady(kind: ProviderKind) {
  agents.set({ popover: null })
  setOverride(kind, 'assumed')
}

export const openPopover = (kind: ProviderKind | null) => agents.set({ popover: kind })
