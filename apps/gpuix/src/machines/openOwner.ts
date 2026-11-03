// The client a thread opens on (ADE-124). A thread on another machine used to fail at once with "This machine is
// offline" whenever this app's own connection to that machine was down, even while the daemon the user is at
// reported it online. Opening it now connects to the machine first (out of any backoff) and waits a little for it.
import type { KybernClient } from '../../../../packages/kybern-client/src/client.ts'
import { OfflineError } from '../../../../packages/kybern-client/src/machines.ts'
import type { MachineId } from '../../../../packages/kybern-client/src/types.ts'

export interface OpenOwnerDeps {
  /** The machine's client now; throws `OfflineError` while it is not connected. */
  clientFor(machineId: MachineId): KybernClient
  /** Connect to the machine and resolve with its client, or reject with `OfflineError` (`MachineHub.reachMachine`). */
  reach(machineId: MachineId): Promise<KybernClient>
  /** The pane still shows this thread: an answer that arrives after the user moved on is dropped. */
  wanted(): boolean
  /** The machine is not connected and the open waits for it. */
  waiting?(): void
}

export type OpenOwner =
  | { kind: 'ready'; owner: KybernClient }
  /** Not reachable: show the cached copy and say so. */
  | { kind: 'offline'; error: unknown }
  /** The user opened something else meanwhile. */
  | { kind: 'gone' }

/**
 * `machine` undefined is the machine the user is at, whose client is `local`. A machine that is connected (or unknown)
 * answers synchronously, so an open that needs no wait keeps its timing; only one that must connect first is a promise.
 */
export function openOwner(machine: MachineId | undefined, local: KybernClient, deps: OpenOwnerDeps): OpenOwner | Promise<OpenOwner> {
  if (!machine) return { kind: 'ready', owner: local }
  try {
    return { kind: 'ready', owner: deps.clientFor(machine) }
  } catch (error) {
    // An unknown machine cannot be reached by waiting.
    if (!(error instanceof OfflineError)) return { kind: 'offline', error }
  }
  return waitFor(machine, deps)
}

async function waitFor(machine: MachineId, deps: OpenOwnerDeps): Promise<OpenOwner> {
  deps.waiting?.()
  try {
    const owner = await deps.reach(machine)
    return deps.wanted() ? { kind: 'ready', owner } : { kind: 'gone' }
  } catch (error) {
    return deps.wanted() ? { kind: 'offline', error } : { kind: 'gone' }
  }
}
