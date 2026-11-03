// The first-run flow's rules, pure: when it opens, which agent is the default, what the agents status line says, what an
// import mode asks the daemon for and what a window-material option writes. No GPUIX, no daemon: the scenes read these and
// `model.test.ts` checks them (`.kybern-artifacts/onboarding-cinematic/SPEC.md` §3d, §4, §5, §12).
import { ANTIGRAVITY_LOGIN_COMMAND, antigravityInstallCommand } from '../../../../packages/kybern-client/src/antigravity.ts'
import { hasSignIn } from '../../../../packages/kybern-client/src/machineAgents.ts'
import type { AccountSummary, ProviderKind, ProviderStatus } from '../../../../packages/kybern-client/src/types.ts'
import { BLUR_MAX, DEFAULT_VARIANT_MATERIAL, defaultBlur, type VariantMaterial } from '../platform/translucency.ts'

// ─── The gate ─────────────────────────────────────────────────────────────

export interface GateInput {
  /** The project and thread lists have arrived. */
  listsLoaded: boolean
  connected: boolean
  projects: number
  threads: number
  /** A thread is open (a restored window). */
  threadOpen: boolean
  /** `onboarding.completed`: finished or skipped once on this environment. */
  completed: boolean
  /** `KYBERN_ONBOARDING=1`: open it again on an existing data directory. */
  forced: boolean
}

/** Brand-new installs only: nothing to open, nothing finished before. The developer flag opens it regardless. */
export function shouldOnboard(g: GateInput): boolean {
  if (!g.listsLoaded || !g.connected) return false
  if (g.forced) return true
  return !g.completed && g.projects === 0 && g.threads === 0 && !g.threadOpen
}

/** A window with data and no completion mark is a returning user: record it so deleting everything later does not bring the flow back. */
export const isReturningUser = (g: Pick<GateInput, 'projects' | 'threads' | 'completed'>) => !g.completed && (g.projects > 0 || g.threads > 0)

// ─── Lists and plurals ────────────────────────────────────────────────────

const plural = new Intl.PluralRules('en')
/** `1 agent`, `4 agents`: the plural rule, the count through `Intl`. */
export function count(n: number, one: string, other = `${one}s`): string {
  return `${new Intl.NumberFormat('en').format(n)} ${plural.select(n) === 'one' ? one : other}`
}
/** The noun alone, for a rolling number that renders the digits itself. */
export const noun = (n: number, one: string, other = `${one}s`) => (plural.select(n) === 'one' ? one : other)
/** `Claude Code, Codex and pi`. */
export const list = (items: string[]) => new Intl.ListFormat('en-US', { style: 'long', type: 'conjunction' }).format(items)

// ─── Agents ───────────────────────────────────────────────────────────────

/** Provider order: it ranks the default agent and lays the tiles out clockwise from the top left. */
export const AGENT_ORDER: readonly ProviderKind[] = ['claude-code', 'codex', 'opencode', 'cursor', 'omp', 'pi', 'antigravity']
export const AGENT_NAMES: Record<ProviderKind, string> = { 'claude-code': 'Claude Code', codex: 'Codex', opencode: 'OpenCode', pi: 'pi', omp: 'Oh My Pi', cursor: 'Cursor', antigravity: 'Antigravity' }

export type AgentState = 'checking' | 'ready' | 'signin' | 'unknown' | 'models' | 'missing' | 'waiting' | 'installing' | 'update'

/** The words on a tile (SPEC §3b). */
export const STATE_WORD: Record<AgentState, string> = {
  checking: 'Checking…', ready: 'Ready', signin: 'Sign in', unknown: 'Check sign-in', models: 'No models', missing: 'Not installed',
  waiting: 'Waiting for browser…', installing: 'Installing…', update: 'Needs an update',
}

/** Models an agent offers without any setup, which say nothing about the providers set up in it (omp lists Apple's on-device model on every Mac). */
const ON_DEVICE_BACKENDS: ReadonlySet<string> = new Set(['apple'])

/** The agent lists a model of a provider the user set up in it. */
export const listsModels = (status: Pick<ProviderStatus, 'models'>): boolean => (status.models ?? []).some((model) => !ON_DEVICE_BACKENDS.has(model.provider ?? ''))

/**
 * What the daemon says about one agent, as a tile state. `loaded` is false until `providers.list` has answered.
 * omp, OpenCode and pi have no account: installed with models listed is ready, whatever an account probe says.
 */
export function deriveAgent(status: ProviderStatus | undefined, accounts: readonly AccountSummary[], loaded: boolean): AgentState {
  if (!loaded || !status) return 'checking'
  if (!status.available) return status.binary_path ? 'update' : 'missing'
  if (!hasSignIn(status.kind)) return listsModels(status) ? 'ready' : 'models'
  if (accounts.some((a) => a.status === 'signed_in')) return 'ready'
  if (accounts.some((a) => a.status === 'needs_sign_in' || a.status === 'signed_out')) return 'signin'
  return 'unknown'
}

/** What the user is doing to a tile right now outranks what the daemon last said. */
export type AgentOverride = 'waiting' | 'installing' | 'assumed' | null
export const applyOverride = (state: AgentState, override: AgentOverride): AgentState =>
  override === 'waiting' ? 'waiting' : override === 'installing' ? 'installing' : override === 'assumed' && (state === 'unknown' || state === 'signin' || state === 'models') ? 'ready' : state

/** Until the user picks one, the default follows provider order, so it can hop to a better-ranked agent as detection finishes. */
export function pickDefault(ready: readonly ProviderKind[], userPick: ProviderKind | null): ProviderKind | null {
  if (userPick && ready.includes(userPick)) return userPick
  return AGENT_ORDER.find((kind) => ready.includes(kind)) ?? null
}

/** "this Mac", "this PC", "this computer", or the machine's name when remote. */
export function hostLabel(platform: string, remoteName?: string | null): string {
  if (remoteName) return remoteName
  return platform === 'darwin' ? 'this Mac' : platform === 'win32' ? 'this PC' : 'this computer'
}

/** The live line under "Your agents, in one place." (SPEC §3d). Names the agents Kybern uses side by side, never just the default. */
export function statusLine(states: Partial<Record<ProviderKind, AgentState>>, defaultAgent: ProviderKind | null, host: string): string {
  const kinds = AGENT_ORDER.filter((kind) => states[kind] !== undefined)
  const ready = kinds.filter((kind) => states[kind] === 'ready')
  const name = (kind: ProviderKind) => AGENT_NAMES[kind]
  const first = defaultAgent ?? ready[0]
  if (ready.length === 0) {
    return kinds.some((kind) => states[kind] === 'checking') ? `Looking for agents on ${host}…` : 'Install or sign in to an agent to continue. Click one to start.'
  }
  const signIn = kinds.filter((kind) => states[kind] === 'signin' || states[kind] === 'unknown' || states[kind] === 'waiting')
  if (ready.length === 1) {
    if (signIn.length === 0) return `${name(ready[0]!)} is ready. Add more agents anytime to use them side by side.`
    return `${name(ready[0]!)} is ready. Sign in to ${list(signIn.map(name))} to use ${signIn.length === 1 ? 'both' : 'them all'}, side by side.`
  }
  const start = first ? ` New chats start with ${name(first)}.` : ''
  if (ready.length === 2) return `You can use ${list(ready.map(name))} side by side.${start}`
  return `All ${ready.length} agents are ready to use side by side.${start}`
}

/** The primary on the agents scene: "Continue with 3 agents", plain "Continue" while none is ready. */
export const continueLabel = (ready: number) => (ready === 0 ? 'Continue' : `Continue with ${count(ready, 'agent')}`)

/** The sentence in the popover of a ready agent (SPEC §3f). */
export const POPOVER_DEFAULT = 'New chats start with this agent. Switch agents in any chat.'

/** The command a terminal sheet types and never runs (the drivers' `unavailable_reason` hints; SPEC v1 §5). */
export function agentCommand(kind: ProviderKind, mode: 'install' | 'login' | 'update', platform: string = process.platform): string | null {
  // Antigravity installs and updates itself with its own script; signing in is the bare `agy`.
  if (kind === 'antigravity') return mode === 'login' ? ANTIGRAVITY_LOGIN_COMMAND : antigravityInstallCommand(platform)
  const packages: Partial<Record<ProviderKind, string>> = {
    'claude-code': '@anthropic-ai/claude-code', codex: '@openai/codex', opencode: 'opencode-ai', pi: '@earendil-works/pi-coding-agent', omp: '@oh-my-pi/pi-coding-agent',
  }
  // omp, OpenCode and pi have no sign-in of their own: the user sets their providers up inside them.
  if (mode === 'login') return ({ 'claude-code': 'claude auth login', codex: 'codex login' } as Partial<Record<ProviderKind, string>>)[kind] ?? null
  const pkg = packages[kind]
  return pkg ? `npm install -g ${pkg}${mode === 'update' ? '@latest' : ''}` : null
}

// ─── Chats to bring in ────────────────────────────────────────────────────

export type ImportMode = 'latest' | 'all' | 'none'
export const LATEST_CHATS = 50
/** The `limit` of a `sessions.import_all` root: the most recent N (50), every chat (`null`), or the project with no chats (0). */
export const limitFor = (mode: ImportMode): number | null => (mode === 'latest' ? LATEST_CHATS : mode === 'all' ? null : 0)

// ─── Window material ──────────────────────────────────────────────────────

export type MaterialOption = 'solid' | 'sidebar' | 'full'
export const MATERIAL_LABEL: Record<MaterialOption, string> = { solid: 'Solid', sidebar: 'Translucent sidebar', full: 'Full translucency' }
export const MATERIAL_CAPTION: Record<MaterialOption, string> = {
  solid: 'An opaque window. Nothing behind Kybern shows through.',
  sidebar: 'Your desktop shows through the sidebar. The conversation stays solid.',
  full: 'Your desktop shows through the whole window. It uses more GPU while agents stream. On Windows, Kybern uses Acrylic.',
}

/**
 * The values each option writes for the active variant (SPEC §5). Translucent sidebar is the shipped default itself
 * (`DEFAULT_VARIANT_MATERIAL`: 90 / 100, Elements at its recipe default, the platform's default blur), so choosing it on a
 * fresh install changes nothing. Full translucency is 85 / 85 / 60 with blur 100 % = `BLUR_MAX`; on Windows that is Acrylic,
 * where the sidebar alone keeps Mica (null).
 */
export function materialPatch(option: MaterialOption, platform: string = process.platform): Partial<VariantMaterial> {
  if (option === 'solid') return { material: 'solid' }
  if (option === 'sidebar') {
    const d = DEFAULT_VARIANT_MATERIAL
    return { material: 'translucent', sidebarOpacity: d.sidebarOpacity, contentOpacity: 100, lastContentOpacity: d.lastContentOpacity, surfaceOpacity: d.surfaceOpacity, blur: defaultBlur(platform as NodeJS.Platform) }
  }
  return { material: 'translucent', sidebarOpacity: 85, contentOpacity: 85, surfaceOpacity: 60, blur: BLUR_MAX }
}

/** Which option a saved material is. Content at 100 is "Sidebar only", below it the whole window. */
export function optionOf(m: Pick<VariantMaterial, 'material' | 'contentOpacity'>): MaterialOption {
  return m.material === 'solid' ? 'solid' : m.contentOpacity >= 100 ? 'sidebar' : 'full'
}

/** Dragging Content below 100 switches to Full translucency, back to 100 switches to Translucent sidebar. */
export function contentSlider(value: number): { option: MaterialOption; patch: Partial<VariantMaterial> } {
  const content = Math.round(value)
  return { option: content < 100 ? 'full' : 'sidebar', patch: { material: 'translucent', contentOpacity: content } }
}

/**
 * "Sidebar 90% · Content 100% · Elements Default · Blur 100%": the closed Adjust row. `elements` is what is applied (held at the
 * readability floor); a value still at its default reads "Default" (Elements) or "Auto" (Blur) instead of a number it does not own.
 */
export function materialSummary(m: Pick<VariantMaterial, 'sidebarOpacity' | 'contentOpacity' | 'blur'>, elements: number | null, showBlur = true): string {
  const parts = [`Sidebar ${m.sidebarOpacity}%`, `Content ${m.contentOpacity}%`, `Elements ${elements === null ? 'Default' : `${Math.round(elements)}%`}`]
  if (showBlur) parts.push(`Blur ${m.blur === null ? 'Auto' : `${blurPercent(m.blur)}%`}`)
  return parts.join(' · ')
}
/** Whether the Window section can do anything here: Linux and Windows 10 draw Kybern’s window opaque (`DESKTOP_MATERIAL === 'none'`). */
export const windowMaterialAvailable = (desktopMaterial: 'vibrancy' | 'dwm' | 'none') => desktopMaterial !== 'none'
export const blurPercent = (blur: number | null) => (blur === null ? 100 : Math.round((blur / BLUR_MAX) * 100))
export const blurFromPercent = (percent: number) => Math.max(1, Math.round((percent / 100) * BLUR_MAX))
