// The network as Settings → Machines and the sidebar footer read it: `network.status` from the machine the
// user is at (any member answers for all of them), `machines.info` from each machine about itself, and the
// actions of design §4.1. One store; it reads while something shows it and follows `network.changed`.
import { useEffect, useSyncExternalStore } from 'react'
import { NETWORK_CHANGED_NOTIFICATION, NETWORK_MACHINE_CHANGED_NOTIFICATION, OUTBOX_CHANGED_NOTIFICATION, type DoctorReport, type MachineId, type MachineInfo, type NetworkAddResult, type NetworkCodeCreateResult, type NetworkRemoveParams, type NetworkStatus, type NetworkUpdateAllResult, type OutboxItem } from '../../../../packages/kybern-client/src/types.ts'
import { getClient } from '../state.ts'
import { machineHub } from './index.ts'

export interface NetworkState {
  /** `null` until the first answer; stays `null` on a daemon without the network. */
  status: NetworkStatus | null
  /** This daemon does not know the network (older than 0.19) or refused the read. */
  unavailable: boolean
  error: string
  /** Each machine's own report of its hardware and harnesses. `false` while it cannot answer. */
  infos: Record<MachineId, MachineInfo | false>
  /** An action is running (its machine id, or the action name for network-wide ones). */
  busy: string | null
  /** Work waiting for, or stuck on its way to, another machine (the outbox list); empty while nothing waits. */
  outbox: OutboxItem[]
  /** What the last "Update all machines" did on each machine, until the page is left. */
  updated: NetworkUpdateAllResult | null
  /** What the last "Run check" found, kept until the next one so the list can be read and acted on. */
  doctor: DoctorReport | null
}

let state: NetworkState = { status: null, unavailable: false, error: '', infos: {}, busy: null, outbox: [], updated: null, doctor: null }
const listeners = new Set<() => void>()
const emit = (patch: Partial<NetworkState>) => { state = { ...state, ...patch }; for (const listener of listeners) listener() }
export const getNetwork = () => state
/** Tests: forget everything. */
export function resetNetwork() { state = { status: null, unavailable: false, error: '', infos: {}, busy: null, outbox: [], updated: null, doctor: null }; reading = undefined; again = false }

const text = (error: unknown) => (error instanceof Error ? error.message : String(error))

let reading: Promise<void> | undefined
let again = false
/** Read `network.status` from the machine the user is at. Reads that arrive during a read fold into one rerun. */
export async function refreshNetwork(): Promise<void> {
  if (reading) { again = true; return reading }
  const run = (async () => {
    do {
      again = false
      const client = getClient()
      if (!client || client.status !== 'open') return
      try {
        const status = await client.call('network.status', {})
        if (getClient() !== client) { again = true; continue }
        emit({ status, unavailable: false, error: '' })
      } catch (error) {
        if (getClient() !== client) { again = true; continue }
        // Method not found: a daemon older than the network.
        emit({ unavailable: /not found|unknown method|-32601/i.test(text(error)), error: text(error) })
      }
    } while (again)
  })()
  const pending = run.finally(() => { if (reading === pending) reading = undefined })
  reading = pending
  return pending
}

/** The work that has not reached its machine yet (queued, sending, failed or expired). */
export async function loadOutbox(): Promise<void> {
  const client = getClient()
  if (!client || client.status !== 'open') return
  try {
    const { items } = await client.call('outbox.list', {})
    if (getClient() === client) emit({ outbox: items })
  } catch { /* a daemon without the outbox: nothing waits */ }
}

/** One machine's `machines.info`, read from that machine (the page asks for the machines it shows). */
export async function loadMachineInfo(machineId: MachineId): Promise<void> {
  try {
    const info = await machineHub().clientFor(machineId).call('machines.info', {})
    emit({ infos: { ...state.infos, [machineId]: info } })
  } catch {
    emit({ infos: { ...state.infos, [machineId]: false } })
  }
}

/** Follow the network while a surface shows it. */
export function useNetwork(): NetworkState {
  const value = useSyncExternalStore((fn) => { listeners.add(fn); return () => { listeners.delete(fn) } }, () => state)
  useEffect(() => {
    const client = getClient()
    void refreshNetwork()
    if (!client) return
    let timer: ReturnType<typeof setTimeout> | undefined
    const soon = () => { clearTimeout(timer); timer = setTimeout(() => void refreshNetwork(), 200) }
    void loadOutbox()
    let outboxTimer: ReturnType<typeof setTimeout> | undefined
    const outboxSoon = () => { clearTimeout(outboxTimer); outboxTimer = setTimeout(() => void loadOutbox(), 200) }
    const stops = [client.onNotification(NETWORK_CHANGED_NOTIFICATION, soon), client.onNotification(NETWORK_MACHINE_CHANGED_NOTIFICATION, soon), client.onNotification(OUTBOX_CHANGED_NOTIFICATION, () => { soon(); outboxSoon() })]
    const off = machineHub().onMachinesChanged(soon)
    return () => { clearTimeout(timer); clearTimeout(outboxTimer); off(); for (const stop of stops) stop() }
  }, [])
  return value
}

// ---- actions: each runs on the machine the user is at (a member), then reads the network again ----

async function act<T>(key: string, run: (client: NonNullable<ReturnType<typeof getClient>>) => Promise<T>): Promise<T> {
  const client = getClient()
  if (!client) throw new Error('Not connected to kybernd')
  emit({ busy: key, error: '' })
  try {
    const result = await run(client)
    await refreshNetwork()
    // The set learns who joined or left from any member.
    await machineHub().set.syncMembers()
    return result
  } catch (error) {
    emit({ error: text(error) })
    throw error
  } finally { emit({ busy: null }) }
}

export const addFound = (foundId: string): Promise<NetworkAddResult> => act(`found:${foundId}`, (c) => c.call('network.add', { found_id: foundId }))
export const mergeFound = (foundId: string) => act(`found:${foundId}`, (c) => c.call('network.merge', { found_id: foundId }))
/** "Add by code": a member joins the machine whose code and address the user typed. */
export const joinByCode = (code: string, address: string, migrate = false) => act('join', (c) => c.call('network.join', { code: code.trim(), address: address.trim(), ...(migrate ? { migrate } : {}) }))
export const pauseMachine = (machineId: MachineId, paused: boolean) => act(machineId, (c) => c.call('network.pause', { machine_id: machineId, paused }))
export const renameMachine = (machineId: MachineId, name: string) => act(machineId, (c) => c.call('network.rename', { machine_id: machineId, name }))
export const removeFromNetwork = (params: NetworkRemoveParams) => act(params.machine_id, (c) => c.call('network.remove', params))
export const setNetworkEnabled = (enabled: boolean) => act('enabled', async (c) => {
  const setup = await import('./setupRuntime.ts')
  if (!enabled) setup.cancelMachineSetup()
  const result = await c.call('network.set_enabled', { enabled })
  if (enabled) setup.startMachineSetup()
  return result
})
export const updateAllMachines = (): Promise<NetworkUpdateAllResult> => act('update', async (c) => { const updated = await c.call('network.update_all', {}); emit({ updated }); return updated })
/** "Keep all machines on the same version": a member that sees a newer version updates itself when idle. */
export const setFollowUpdates = (enabled: boolean) => act('follow', (c) => c.call('network.follow_updates.set', { enabled }))
/** "Keep this machine reachable when Kybern is closed": the daemon registers or removes its login item before answering. */
export const setKeepReachable = (enabled: boolean) => act('reachable', (c) => c.call('network.keep_reachable.set', { enabled }))
export const retryOutbox = (id: string) => act(`outbox:${id}`, async (c) => { const item = await c.call('outbox.retry', { id }); await loadOutbox(); return item })
export const cancelOutbox = (id: string) => act(`outbox:${id}`, async (c) => { const item = await c.call('outbox.cancel', { id }); await loadOutbox(); return item })
export const doctor = (machineId?: MachineId): Promise<DoctorReport> => act('doctor', async (c) => { const report = await c.call('network.doctor', { machine_id: machineId ?? null }); emit({ doctor: report }); return report })
/** "Show a code for this machine": the code another machine's member types, on a machine that is not in a network yet. */
export const showCode = (): Promise<NetworkCodeCreateResult> => act('code', (c) => c.call('network.code.create', {}))
