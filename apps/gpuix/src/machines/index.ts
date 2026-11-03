// The app's machine hub and the lookups the rest of GPUIX uses. The hub is made
// on first use from the saved machines (`machines.json`, migrated once from
// `environments.json`); state.ts feeds it the primary connection (see hub.ts).
import { useMemo, useSyncExternalStore } from 'react'
import type { KybernClient } from '../../../../packages/kybern-client/src/client.ts'
import { MachineCache } from '../../../../packages/kybern-client/src/machineCache.ts'
import { machineGlyph, showMachineGlyph, type MachineGlyph } from '../../../../packages/kybern-client/src/machineMerge.ts'
import type { MachineConnectionStatus, MergedView } from '../../../../packages/kybern-client/src/machines.ts'
import { configDir, localProfile, readLegacyRegistry } from '../environments/registry.ts'
import { readCredential, removeCredential, writeCredential } from '../environments/credentials.ts'
import { ensureSshTunnel } from '../environments/ssh.ts'
import { MachineHub } from './hub.ts'
import { fileCacheStorage, machineCredentials, migrateEnvironments, writeMachineRecords, type GpuixMachineRecord } from './store.ts'
import { staleSelfRecords } from './staleSelf.ts'

let hub: MachineHub | undefined
export function machineHub(): MachineHub {
  if (hub) return hub
  const dir = configDir()
  // Read once, when the set loads: `machines.json`, made from `environments.json` the first time.
  const loaded = () => migrateEnvironments({
    dir,
    registry: () => { try { return readLegacyRegistry() } catch { return null } },
    local: localProfile,
    read: readCredential, write: writeCredential,
  }).then((result) => result.records)
  const current = new MachineHub({
    records: loaded,
    save: (records) => writeMachineRecords(dir, records),
    credentials: machineCredentials({
      read: readCredential, write: writeCredential, remove: removeCredential,
      legacyProfileOf: (machineId) => current.set.connection(machineId)?.record.legacyProfileId,
    }),
    cache: new MachineCache(fileCacheStorage(dir)),
    // A saved SSH machine is reached through a tunnel the app keeps open.
    prepare: async (record) => {
      const ssh = (record as GpuixMachineRecord).ssh
      if (!ssh || !record.legacyProfileId) return
      const port = await ensureSshTunnel(record.legacyProfileId, ssh)
      const route = record.routes.find((item) => item.kind === 'ssh')
      if (route) route.url = `http://127.0.0.1:${port}`
    },
    // The profile-id accounts stay: releases without machines read them. `credentials.delete` removes both when the user forgets a machine.
    readCredential,
  })
  hub = current
  return current
}
/** Tests: forget the hub so the next use builds a fresh one. */
export function resetMachineHub() { hub?.dispose(); hub = undefined; glyphMemo = undefined }

/** The merged projects and threads of every machine. */
export function useMachineView(): MergedView {
  const h = machineHub()
  return useSyncExternalStore(h.subscribe.bind(h), () => h.view())
}

/** A machine as the sidebar and Settings show it. */
export interface MachineRow {
  machineId: string
  name: string
  status: MachineConnectionStatus
  /** Not reachable now (offline, paused or unpaired): its threads are cached copies. */
  offline: boolean
  isLocal: boolean
  lastSeenAt?: number
  glyph: MachineGlyph
  /** Show the glyph on this machine's threads. */
  showGlyph: boolean
  member: boolean
  paused: boolean
  os?: string
  relayVia?: string
  version?: string
  error?: string
  /** This app is connecting to it now. */
  connecting?: boolean
  /** This app reaches it, but the daemon of this computer has no session to it (`daemonLinkDown`). */
  linkDown?: boolean
}
const LIVE = new Set<MachineConnectionStatus>(['online', 'relaying', 'needs-update'])

export function machineRows(h: MachineHub): MachineRow[] {
  const local = h.localMachineId
  const all = h.connections()
  const stale = staleSelfRecords(all, local)
  const connections = stale.size ? all.filter((connection) => !stale.has(connection.record.machineId)) : all
  const count = connections.length
  const glyphs = h.set.glyphs()
  return connections.map((connection) => {
    const { machineId } = connection.record
    const name = h.set.displayName(machineId)
    return {
      machineId, name,
      status: connection.status,
      offline: !LIVE.has(connection.status),
      isLocal: machineId === local,
      ...(connection.lastSeenAt !== undefined ? { lastSeenAt: connection.lastSeenAt } : {}),
      glyph: glyphs.get(machineId) ?? machineGlyph({ machineId, name }),
      showGlyph: showMachineGlyph({ machineCount: count, machineId, ...(local ? { localMachineId: local } : {}) }),
      member: connection.record.member,
      paused: !!connection.record.paused,
      ...(connection.record.os ? { os: connection.record.os } : {}),
      ...(connection.relayVia ? { relayVia: connection.relayVia } : {}),
      ...(connection.info?.version ? { version: connection.info.version } : {}),
      ...(connection.error ? { error: connection.error.message } : {}),
      ...(connection.connecting ? { connecting: true } : {}),
    }
  })
}

/**
 * The machines, without the lists. Cheap: it only reads machines and statuses, so with one machine
 * nothing but the status dot ever re-renders (the merged lists are not kept for it).
 */
export function useMachines(): MachineRow[] {
  const h = machineHub()
  const version = useSyncExternalStore(h.onMachinesChanged.bind(h), () => h.set.version)
  return useMemo(() => machineRows(h), [h, version])
}

let glyphMemo: { version: number; glyphs: Map<string, MachineGlyph> } | undefined
/** A machine's glyph by id: named from its record, with a letter no other machine of the set uses. */
export function glyphOfMachine(machineId: string): MachineGlyph {
  const h = machineHub()
  if (glyphMemo?.version !== h.set.version) glyphMemo = { version: h.set.version, glyphs: h.set.glyphs() }
  return glyphMemo.glyphs.get(machineId) ?? machineGlyph({ machineId })
}

export const clientFor = (machineId: string): KybernClient => machineHub().clientFor(machineId)
export const clientForThread = (threadId: string, machineId?: string): KybernClient => machineHub().clientForThread(threadId, machineId)
export const clientForProject = (projectId: string, machineId?: string): KybernClient => machineHub().clientForProject(projectId, machineId)
