// A terminal with a command typed into it and never run: the person reads it and presses Return. Used by the agents
// scene's terminal sheet and by the GitHub CLI setup on the pull request surfaces.
import type { TerminalInfo } from '../../../../packages/kybern-client/src/types.ts'
import type { getClient } from '../state.ts'

type Client = NonNullable<ReturnType<typeof getClient>>

const b64 = (text: string) => Buffer.from(text).toString('base64')

export interface TypedTerminalOptions {
  cwd: string
  rows?: number
  /** Whether the terminal is still wanted once it exists (the sheet may have closed meanwhile); a `false` closes it. */
  wanted: (info: TerminalInfo) => boolean
  /** The caller's timer, so its reset can drop pending typing. */
  later: (ms: number, fn: () => void) => void
}

/** Open a shell and type `command` the way a person would, once the shell has drawn its prompt. `null` when it was no longer wanted. */
export async function openTypedTerminal(client: Client, command: string, { cwd, rows = 14, wanted, later }: TypedTerminalOptions): Promise<TerminalInfo | null> {
  const info = await client.call('terminals.create', { cols: 100, rows, cwd })
  if (!wanted(info)) { void client.call('terminals.close', { terminal_id: info.id }).catch(() => {}); return null }
  // Typed before the prompt, the tty would echo it twice.
  let i = 0
  let started = false
  const type = () => {
    if (!wanted(info) || i >= command.length) return
    const chunk = command.slice(i, i + 2)
    i += chunk.length
    void client.call('terminals.input', { terminal_id: info.id, data: b64(chunk) }).catch(() => {})
    later(22, type)
  }
  const start = () => { if (started) return; started = true; off(); type() }
  let quiet: ReturnType<typeof setTimeout> | undefined
  const off = client.onNotification('terminal.output', (params) => {
    if ((params as { terminal_id?: string }).terminal_id !== info.id) return
    clearTimeout(quiet)
    quiet = setTimeout(start, 350)
  })
  later(3000, start)
  return info
}
