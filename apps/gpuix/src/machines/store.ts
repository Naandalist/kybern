// Where GPUIX keeps its machines (design §5.2, "Migration of environments.json").
//
// - `machines.json` in the client config dir holds one `MachineRecord` per
//   machine. A record made from a saved SSH profile keeps its `ssh` settings so
//   the app can open the tunnel before it dials.
// - Device tokens live in the credential store under `machine:<machine id>`. A
//   record that came from a saved profile reads that profile's token until its
//   machine id is verified (the set moves it then, see `MachineSet.rekeyMachine`).
// - `migrateEnvironments` turns the old `environments.json` into `machines.json` the
//   first time. It is idempotent: with `machines.json` present nothing is read or written.
// - Coexistence: releases without machines read only `environments.json` and the profile-id
//   credential accounts. A newer build therefore never renames or removes them: tokens are
//   copied to `machine:<id>`, the profile-id copy stays (and follows a replaced token) until
//   the user forgets the machine, and `syncEnvironments` keeps the profiles in step.
import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from 'node:fs'
import path from 'node:path'
import type { CredentialStore, MachineRecord } from '../../../../packages/kybern-client/src/machines.ts'
import type { CacheStorage } from '../../../../packages/kybern-client/src/machineCache.ts'
import type { EnvironmentProfile, Registry } from '../environments/registry.ts'
import { writeAllowed } from '../shell/persist.ts'
import { isLegacyMachineId, recordsFromRegistry } from './records.ts'

export type SavedSsh = NonNullable<EnvironmentProfile['ssh']>
/** A record as GPUIX keeps it: the SSH settings of a machine reached through a tunnel. */
export type GpuixMachineRecord = MachineRecord & { ssh?: SavedSsh }

const VERSION = 1
export const MACHINES_FILE = 'machines.json'
export const MACHINE_CACHE_FILE = 'machine-cache.json'
export const ENVIRONMENTS_FILE = 'environments.json'
export const machineCredentialId = (machineId: string) => `machine:${machineId}`

function writeJson(file: string, value: unknown) {
  if (!writeAllowed(file)) return
  mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 })
  const temp = `${file}.${process.pid}.tmp`
  writeFileSync(temp, JSON.stringify(value, null, 2), { mode: 0o600 })
  renameSync(temp, file)
}

function isRecord(value: unknown): value is GpuixMachineRecord {
  const record = value as GpuixMachineRecord | null
  return !!record && typeof record === 'object' && typeof record.machineId === 'string' && typeof record.name === 'string'
}

/** The saved records, or `null` when there is no `machines.json` yet. An unreadable file is an error: a later save must not replace it. */
export function readMachineRecords(dir: string): GpuixMachineRecord[] | null {
  const file = path.join(dir, MACHINES_FILE)
  let text: string
  try { text = readFileSync(file, 'utf8') } catch (e) {
    if ((e as NodeJS.ErrnoException).code === 'ENOENT') return null
    throw new Error('Unable to read saved machines. Restore machines.json from a backup before saving connections.')
  }
  const value = JSON.parse(text) as { machines?: unknown }
  if (!Array.isArray(value.machines)) throw new Error('Invalid machines file')
  return value.machines.filter(isRecord).map((record) => ({ ...record, routes: Array.isArray(record.routes) ? record.routes : [] }))
}

export function writeMachineRecords(dir: string, records: readonly MachineRecord[]) {
  const previous = (() => { try { return readMachineRecords(dir) } catch { return null } })()
  writeJson(path.join(dir, MACHINES_FILE), { version: VERSION, machines: records })
  syncEnvironments(dir, previous, records as readonly GpuixMachineRecord[])
}

/**
 * Best effort: keep `environments.json` (read by releases without machines) in step. A record that was saved
 * from a profile and is now gone loses its profile; a profile added by the new app is not written, because the
 * older app would have no token for it. Only a file that exists is touched, and an unreadable one is left alone.
 */
function syncEnvironments(dir: string, previous: readonly GpuixMachineRecord[] | null, records: readonly GpuixMachineRecord[]) {
  const file = path.join(dir, ENVIRONMENTS_FILE)
  if (!previous || !existsSync(file) || !writeAllowed(file)) return
  const kept = new Set(records.map((record) => record.legacyProfileId).filter(Boolean))
  const gone = new Set(previous.map((record) => record.legacyProfileId).filter((id): id is string => !!id && !kept.has(id)))
  if (!gone.size) return
  try {
    const value = JSON.parse(readFileSync(file, 'utf8')) as Registry
    if (!Array.isArray(value.environments)) return
    const environments = value.environments.filter((profile) => !gone.has(profile.id))
    if (environments.length === value.environments.length) return
    writeJson(file, { ...value, selected_id: environments.some((p) => p.id === value.selected_id) ? value.selected_id : 'local', environments })
  } catch { /* the older app's file stays as it is */ }
}

/** The offline cache's file, written whole each time. */
export function fileCacheStorage(dir: string): CacheStorage {
  const file = path.join(dir, MACHINE_CACHE_FILE)
  return {
    read: () => { try { return readFileSync(file, 'utf8') } catch { return null } },
    write: (text) => { if (!writeAllowed(file)) return; mkdirSync(dir, { recursive: true, mode: 0o700 }); const temp = `${file}.${process.pid}.tmp`; writeFileSync(temp, text, { mode: 0o600 }); renameSync(temp, file) },
  }
}

export interface MigrationDeps {
  dir: string
  /** The saved environments (`readRegistry`), or `null` when the old file is absent. */
  registry: () => Registry | null
  local: EnvironmentProfile
  read: (id: string) => Promise<string>
  write: (id: string, token: string) => Promise<void>
}

export interface MigrationResult { records: GpuixMachineRecord[]; migrated: boolean }

/**
 * `machines.json` as it is, else made from `environments.json`. A profile whose daemon id is known gets a copy of its token under
 * `machine:<id>` now; the rest use theirs under the profile id until the machine answers. The old file and the profile-id
 * tokens stay, so a release without machines keeps working.
 */
export async function migrateEnvironments(deps: MigrationDeps): Promise<MigrationResult> {
  const existing = readMachineRecords(deps.dir)
  if (existing) return { records: existing, migrated: false }
  const registry = deps.registry()
  const records: GpuixMachineRecord[] = recordsFromRegistry(registry ?? { selected_id: 'local', environments: [] }, deps.local)
  if (registry) for (const profile of registry.environments) {
    if (!profile.ssh) continue
    const record = records.find((item) => item.legacyProfileId === profile.id)
    if (record) record.ssh = profile.ssh
  }
  writeMachineRecords(deps.dir, records)
  if (!registry) return { records, migrated: false }
  // A token that cannot be copied (locked keyring) is still found under the profile id, which the record names.
  for (const profile of registry.environments) {
    const record = records.find((item) => item.legacyProfileId === profile.id)
    if (!record || isLegacyMachineId(record.machineId)) continue
    try {
      const token = await deps.read(profile.id)
      await deps.write(machineCredentialId(record.machineId), token)
    } catch { /* The legacy token keeps working through `legacyProfileId`. */ }
  }
  return { records, migrated: true }
}

/**
 * A read that found no token. Only `CredentialMissingError` says so: anything else (a locked store, a file that could
 * not be read) is the store failing, and treating it as "no token" would mint a new one, which revokes the saved one.
 */
function isMissing(error: unknown): boolean {
  return (error as { name?: string } | null)?.name === 'CredentialMissingError'
}

export interface CredentialDeps {
  read: (id: string) => Promise<string>
  write: (id: string, token: string) => Promise<void>
  remove: (id: string) => Promise<void>
  /** A machine's saved profile, for a token that has not moved yet. */
  legacyProfileOf: (machineId: string) => string | undefined
}

/**
 * The set's credentials: `machine:<id>`, then the saved profile's token for a machine that has not moved yet.
 * The profile-id account is the older app's: a new token also replaces it where it still exists, `discard` leaves
 * it, and only `delete` (the user forgot the machine) removes both.
 */
export function machineCredentials(deps: CredentialDeps): CredentialStore {
  // Machines whose token was rejected: the profile copy is the same dead token, so `get` must not hand it back.
  const rejected = new Set<string>()
  return {
    // Only "no such token" is a missing token. A store that fails (a locked Keychain, a dismissed keyring prompt) throws,
    // so the set does not mint a replacement: minting revokes the saved token on that machine, and a replacement the
    // locked store then cannot keep leaves this device with no working token at all (ADE-124).
    get: async (machineId) => {
      try { return await deps.read(machineCredentialId(machineId)) } catch (error) { if (!isMissing(error)) throw error }
      const profile = deps.legacyProfileOf(machineId)
      if (!profile || rejected.has(machineId)) return undefined
      try { return await deps.read(profile) } catch (error) { if (!isMissing(error)) throw error; return undefined }
    },
    set: async (machineId, token) => {
      await deps.write(machineCredentialId(machineId), token)
      rejected.delete(machineId)
      const profile = deps.legacyProfileOf(machineId)
      if (!profile || profile === machineId) return
      // Only an account that exists: a machine added by this build must not leave a token the older app cannot use.
      try { await deps.read(profile) } catch { return }
      await deps.write(profile, token).catch(() => {})
    },
    discard: async (machineId) => {
      await deps.remove(machineCredentialId(machineId)).catch(() => {})
      rejected.add(machineId)
    },
    delete: async (machineId) => {
      const profile = deps.legacyProfileOf(machineId)
      await deps.remove(machineCredentialId(machineId)).catch(() => {})
      if (profile && machineId !== profile) await deps.remove(profile).catch(() => {})
    },
  }
}
