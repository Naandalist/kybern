// Installing and signing in to the GitHub CLI from a pull request surface: the command is typed into a terminal on the
// project's machine (the same sheet the agents scene uses) and runs when the person presses Return. Closing the sheet
// checks again: the daemon finds a gh installed while it ran (`gh_locate`), so no restart is needed.
import { existsSync } from 'node:fs'
import { homedir } from 'node:os'
import type { TerminalInfo } from '../../../../packages/kybern-client/src/types.ts'
import { openTypedTerminal } from '../onboarding/typedTerminal.ts'
import { createStore } from '../onboarding/store.ts'
import type { TerminalSheetState } from '../onboarding/TerminalSheet.tsx'
import { openQuietly } from '../platform/os.ts'
import { activeEnvironment } from '../shell/environment.ts'
import { machineOfProjectId } from '../state.ts'
import { prClient } from './client.ts'

export const GH_INSTALL_URL = 'https://cli.github.com'
/** The sign-in the daemon drives for agents (`login_recipes`, `gh`), typed for the person to run. */
export const GH_SIGN_IN = 'gh auth login -h github.com --web -p https -s repo,read:org,workflow'

/** The command that installs gh here, or `null` when there is no installer to run (the install guide opens instead). */
export function ghInstallCommand(platform: NodeJS.Platform, hasFile: (path: string) => boolean = existsSync): string | null {
  if (platform === 'win32') return 'winget install --id GitHub.cli -e'
  if (platform === 'darwin' && ['/opt/homebrew/bin/brew', '/usr/local/bin/brew'].some(hasFile)) return 'brew install gh'
  return null
}

interface GhSheet extends TerminalSheetState { done: () => void }
export const ghSheet = createStore<{ sheet: GhSheet | null }>({ sheet: null })

/** The machine whose terminal the sheet shows. */
let owner: ReturnType<typeof prClient> = null
const timers = new Set<ReturnType<typeof setTimeout>>()
const later = (ms: number, fn: () => void) => { const id = setTimeout(() => { timers.delete(id); fn() }, ms); timers.add(id) }

/** The project's machine runs the command; only this computer's own OS and installers are known. */
function machineFor(projectId: string) {
  const client = prClient(projectId)
  return { client, local: !!client && machineOfProjectId(projectId) === undefined && activeEnvironment().local }
}

async function open(projectId: string, title: string, command: string, done: () => void) {
  const { client, local } = machineFor(projectId)
  if (!client) return
  owner = client
  ghSheet.set({ sheet: { title, info: null, exited: false, error: null, done } })
  try {
    const wanted = (info: TerminalInfo) => { const live = ghSheet.get().sheet; return !!live && live.title === title && (!live.info || live.info.id === info.id) }
    const info = await openTypedTerminal(client, command, { cwd: local ? homedir() : '/', wanted, later })
    if (info) ghSheet.set((s) => ({ sheet: s.sheet ? { ...s.sheet, info } : s.sheet }))
  } catch (error) {
    ghSheet.set((s) => ({ sheet: s.sheet ? { ...s.sheet, error: error instanceof Error ? error.message : String(error) } : s.sheet }))
  }
}

/** "Install GitHub CLI": winget on Windows, Homebrew on a Mac that has it, else the install guide. `done` runs when the sheet closes. */
export function installGh(projectId: string, done: () => void): void {
  const { local } = machineFor(projectId)
  const command = local ? ghInstallCommand(process.platform) : null
  if (!command) { openQuietly(GH_INSTALL_URL); return }
  void open(projectId, 'Install GitHub CLI', command, done)
}

/** "Sign in to GitHub": gh's own browser sign-in in a terminal. */
export function signInGh(projectId: string, done: () => void): void {
  void open(projectId, 'Sign in to GitHub', GH_SIGN_IN, done)
}

export const ghSheetExited = () => ghSheet.set((s) => ({ sheet: s.sheet ? { ...s.sheet, exited: true } : s.sheet }))

/** Close the sheet and its terminal, then check again. */
export function closeGhSheet(): void {
  const sheet = ghSheet.get().sheet
  if (!sheet) return
  ghSheet.set({ sheet: null })
  for (const id of timers) clearTimeout(id)
  timers.clear()
  if (sheet.info) void owner?.call('terminals.close', { terminal_id: sheet.info.id }).catch(() => {})
  owner = null
  sheet.done()
}
