// A browser tab's lifetime (ADE-128). Closing a thread's last browser tab ends its page: the page, its history, relays,
// tickets, web view and Fast choices go at once, so the next browser tab starts at the servers list. A forward that was opened
// for a preview (the daemon marks it `origin: 'preview'`) is stopped `FORWARD_GRACE_MS` after no open browser shows it any
// more; a browser that shows it again within that time keeps it. Manual forwards, and every forward of a daemon that does not
// mark them (before 0.21.5), are never stopped here, and neither is a forward no browser of this app has shown.
// Nothing here touches React, the daemon or the tab stores: `PreviewIntegration` passes them in.
import type { PortForward } from '../../../../packages/kybern-client/src/types.ts'

export const FORWARD_GRACE_MS = 10_000

/** What one thread's browser shows: the forwarded servers it names (machine and remote port) and the loopback ports it loads. */
export interface PreviewUse { machines: { id: string; port: number }[]; ports: number[] }

/** The slice of a browser's state that names what it shows. */
export interface UseSource {
  entries: readonly { address: string; url: string; frameUrl?: string; machine?: { id: string; port: number } }[]
  attemptedMachine?: { id: string; port: number }
}

const LOOPBACK = new Set(['localhost', '127.0.0.1', '[::1]'])
/** The port of a loopback address (a forward's `http://localhost:<local>` or Fast `https://localhost:<https>`), else null. */
export function loopbackPort(address: string): number | null {
  try {
    const url = new URL(address)
    if (!LOOPBACK.has(url.hostname) || (url.protocol !== 'http:' && url.protocol !== 'https:')) return null
    return Number(url.port) || (url.protocol === 'https:' ? 443 : 80)
  } catch { return null }
}

/** Everything a browser could load from a forward: its history (back and forward open those again) and the server it is opening. */
export function previewUse(state: UseSource): PreviewUse {
  const machines: PreviewUse['machines'] = [], ports: number[] = []
  for (const entry of state.entries) {
    if (entry.machine) machines.push({ id: entry.machine.id, port: entry.machine.port })
    for (const address of [entry.address, entry.url]) { const port = loopbackPort(address); if (port !== null) ports.push(port) }
  }
  if (state.attemptedMachine) machines.push({ id: state.attemptedMachine.id, port: state.attemptedMachine.port })
  return { machines, ports }
}

export const forwardInUse = (forward: Pick<PortForward, 'machine_id' | 'remote_port' | 'local_port' | 'https_port'>, uses: readonly PreviewUse[]): boolean =>
  uses.some(use => use.machines.some(m => m.id === forward.machine_id && m.port === forward.remote_port) || use.ports.some(port => port === forward.local_port || port === forward.https_port))

export interface LifetimeDeps {
  /** Threads that hold browser state. */
  threads(): readonly string[]
  /** A browser tab of the thread is open (on screen or remembered while the thread is off screen), or its page floats. */
  open(threadId: string): boolean
  /** Forget the thread's page: what a new browser tab shows is the servers list. */
  dispose(threadId: string): void
  /** What each open browser shows. */
  uses(): readonly PreviewUse[]
  /** This machine's forwards (`ports.list`, kept current by `ports.changed`). */
  forwards(): readonly PortForward[]
  stop(id: string): Promise<unknown>
  /** Calls `listener` when tabs, browsers or forwards change. */
  subscribe(listener: () => void): () => void
  graceMs?: number
  now?: () => number
  setTimer?: (run: () => void, ms: number) => unknown
  clearTimer?: (timer: unknown) => void
}

/** Start watching; returns the way to stop. Changes are handled together on the next tick, so a tab moved between groups (taken
 *  out of one and put in another in the same update) never reads as closed. */
export function watchPreviewLifetime(deps: LifetimeDeps): () => void {
  const grace = deps.graceMs ?? FORWARD_GRACE_MS, now = deps.now ?? Date.now
  const setTimer = deps.setTimer ?? ((run, ms) => setTimeout(run, ms)), clearTimer = deps.clearTimer ?? (timer => clearTimeout(timer as ReturnType<typeof setTimeout>))
  /** Preview forwards an open browser of this app has shown, and since when none shows them. */
  const shown = new Set<string>(), unusedSince = new Map<string, number>()
  let pending: unknown, due: unknown, stopped = false
  const check = () => {
    pending = undefined
    if (stopped) return
    for (const threadId of deps.threads()) if (!deps.open(threadId)) deps.dispose(threadId)
    const forwards = deps.forwards(), uses = deps.uses(), at = now()
    const live = new Set(forwards.map(forward => forward.id))
    for (const id of [...shown]) if (!live.has(id)) { shown.delete(id); unusedSince.delete(id) }
    let next = Infinity
    for (const forward of forwards) {
      if (forward.origin !== 'preview') { shown.delete(forward.id); unusedSince.delete(forward.id); continue }
      if (forwardInUse(forward, uses)) { shown.add(forward.id); unusedSince.delete(forward.id); continue }
      if (!shown.has(forward.id)) continue
      const since = unusedSince.get(forward.id) ?? at
      unusedSince.set(forward.id, since)
      if (at - since >= grace) { shown.delete(forward.id); unusedSince.delete(forward.id); void deps.stop(forward.id).catch(() => {}) }
      else next = Math.min(next, since + grace)
    }
    if (due !== undefined) { clearTimer(due); due = undefined }
    if (next !== Infinity) due = setTimer(() => { due = undefined; schedule() }, next - at)
  }
  const schedule = () => { if (pending === undefined && !stopped) pending = setTimer(check, 0) }
  const off = deps.subscribe(schedule)
  schedule()
  return () => {
    stopped = true; off()
    if (pending !== undefined) clearTimer(pending)
    if (due !== undefined) clearTimer(due)
  }
}
