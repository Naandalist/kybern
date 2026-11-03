// Saved machines: `machines.json`, the one-time migration of `environments.json`, and where tokens live.
import { afterEach, beforeEach, expect, test } from 'bun:test'
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import type { EnvironmentProfile, Registry } from '../environments/registry.ts'
import { CredentialMissingError, credentialStore } from '../environments/credentials.ts'
import { ENVIRONMENTS_FILE, MACHINES_FILE, machineCredentialId, machineCredentials, migrateEnvironments, readMachineRecords, writeMachineRecords, type MigrationDeps } from './store.ts'

let dir = ''
beforeEach(() => { dir = mkdtempSync(path.join(tmpdir(), 'kybern-machines-')) })
afterEach(() => { rmSync(dir, { recursive: true, force: true }) })

const local: EnvironmentProfile = { id: 'local', name: 'This Mac', local: true }
const registry: Registry = {
  selected_id: 'local',
  environments: [
    { id: 'p-verified', name: 'Dev server', local: false, url: 'wss://dev.example/ws', environment_id: 'env-dev' },
    { id: 'p-ssh', name: 'Box', local: false, ssh: { target: 'me@box', remote_port: 4199, local_port: 4455 } },
  ],
}

function deps(over: Partial<MigrationDeps> = {}) {
  const vault = new Map<string, string>([['p-verified', 'tok-dev'], ['p-ssh', 'tok-box']])
  const calls: string[] = []
  const value: MigrationDeps = {
    dir, local,
    registry: () => registry,
    read: async (id) => { const token = vault.get(id); if (!token) throw new CredentialMissingError('missing'); return token },
    write: async (id, token) => { calls.push(`write ${id}`); vault.set(id, token) },
    ...over,
  }
  return { value, vault, calls }
}

test('the first start makes machines.json from environments.json and leaves the old file and profile tokens for older releases', async () => {
  writeFileSync(path.join(dir, ENVIRONMENTS_FILE), JSON.stringify(registry))
  const { value, vault } = deps()
  const result = await migrateEnvironments(value)
  expect(result.migrated).toBe(true)
  expect(result.records.map((record) => record.name)).toEqual(['This Mac', 'Dev server', 'Box'])
  expect(existsSync(path.join(dir, ENVIRONMENTS_FILE))).toBe(true)
  expect(JSON.parse(readFileSync(path.join(dir, ENVIRONMENTS_FILE), 'utf8'))).toEqual(registry)
  expect(existsSync(path.join(dir, 'environments.json.migrated'))).toBe(false)
  expect(readMachineRecords(dir)!.map((record) => record.machineId)).toEqual(['legacy:local', 'env-dev', 'legacy:p-ssh'])
  // The verified machine's token is copied under machine:<id>; the profile-id accounts stay for the older app.
  expect(vault.get(machineCredentialId('env-dev'))).toBe('tok-dev')
  expect(vault.get('p-verified')).toBe('tok-dev')
  expect(vault.get('p-ssh')).toBe('tok-box')
})

test('an SSH profile keeps its settings and becomes an SSH route', async () => {
  const result = await migrateEnvironments(deps().value)
  const box = result.records.find((record) => record.legacyProfileId === 'p-ssh')!
  expect(box.routes).toEqual([{ kind: 'ssh', url: 'http://127.0.0.1:4455' }])
  expect(box.ssh).toEqual({ target: 'me@box', remote_port: 4199, local_port: 4455 })
  expect(readMachineRecords(dir)!.find((record) => record.legacyProfileId === 'p-ssh')!.ssh).toEqual(box.ssh)
})

test('migrating twice changes nothing: machines.json wins and no token moves again', async () => {
  writeFileSync(path.join(dir, ENVIRONMENTS_FILE), JSON.stringify(registry))
  await migrateEnvironments(deps().value)
  const before = readFileSync(path.join(dir, MACHINES_FILE), 'utf8')
  const again = deps({ registry: () => { throw new Error('the old file must not be read') } })
  const result = await migrateEnvironments(again.value)
  expect(result.migrated).toBe(false)
  expect(again.calls).toEqual([])
  expect(readFileSync(path.join(dir, MACHINES_FILE), 'utf8')).toBe(before)
})

test('a token that cannot move stays where it was and the migration still completes', async () => {
  const { value, vault } = deps({ write: async () => { throw new Error('keyring locked') } })
  const result = await migrateEnvironments(value)
  expect(result.migrated).toBe(true)
  expect(vault.get('p-verified')).toBe('tok-dev')
  // The record still names its profile, so the credentials adapter finds the old token.
  const credentials = machineCredentials({ read: value.read, write: value.write, remove: async () => {}, legacyProfileOf: (id) => result.records.find((record) => record.machineId === id)?.legacyProfileId })
  expect(credentials.get('env-dev')).resolves.toBe('tok-dev')
})

test('a fresh install with no old file starts with the local machine only', async () => {
  const result = await migrateEnvironments(deps({ registry: () => null }).value)
  expect(result.migrated).toBe(false)
  expect(result.records.map((record) => record.name)).toEqual(['This Mac'])
  expect(readMachineRecords(dir)).toHaveLength(1)
})

test('an unreadable machines.json is an error, never a silent restart from nothing', () => {
  writeFileSync(path.join(dir, MACHINES_FILE), '{ not json')
  expect(() => readMachineRecords(dir)).toThrow()
  writeFileSync(path.join(dir, MACHINES_FILE), JSON.stringify({ machines: 'x' }))
  expect(() => readMachineRecords(dir)).toThrow('Invalid machines file')
})

test('credentials: machine:<id> first, the profile token until it moves, both cleaned on delete', async () => {
  const vault = new Map<string, string>([['p1', 'old']])
  const credentials = machineCredentials({
    read: async (id) => { const token = vault.get(id); if (!token) throw new CredentialMissingError('missing'); return token },
    write: async (id, token) => { vault.set(id, token) },
    remove: async (id) => { vault.delete(id) },
    legacyProfileOf: (id) => (id === 'legacy:p1' ? 'p1' : undefined),
  })
  expect(await credentials.get('legacy:p1')).toBe('old')
  expect(await credentials.get('nobody')).toBeUndefined()
  await credentials.set('m1', 'new')
  expect(vault.get('machine:m1')).toBe('new')
  expect(await credentials.get('m1')).toBe('new')
  await credentials.delete('legacy:p1')
  expect(vault.has('p1')).toBe(false)
  await credentials.delete('m1')
  expect(vault.has('machine:m1')).toBe(false)
})

test('records round trip through machines.json', () => {
  writeMachineRecords(dir, [{ machineId: 'a', name: 'A', routes: [{ kind: 'direct_tailnet', url: 'http://a:4199' }], member: true, signingKey: 'k' }])
  expect(readMachineRecords(dir)).toEqual([{ machineId: 'a', name: 'A', routes: [{ kind: 'direct_tailnet', url: 'http://a:4199' }], member: true, signingKey: 'k' }])
})

function vaultCredentials(initial: [string, string][], profiles: Record<string, string>) {
  const vault = new Map<string, string>(initial)
  const credentials = machineCredentials({
    read: async (id) => { const token = vault.get(id); if (!token) throw new CredentialMissingError('missing'); return token },
    write: async (id, token) => { vault.set(id, token) },
    remove: async (id) => { vault.delete(id) },
    legacyProfileOf: (id) => profiles[id],
  })
  return { vault, credentials }
}

test('a replaced token also updates the profile account an older release reads', async () => {
  const { vault, credentials } = vaultCredentials([['p1', 'old'], ['machine:m1', 'old']], { m1: 'p1' })
  await credentials.set('m1', 'fresh')
  expect(vault.get('machine:m1')).toBe('fresh')
  expect(vault.get('p1')).toBe('fresh')
})

test('a token never creates a profile account that was not there', async () => {
  const { vault, credentials } = vaultCredentials([], { m1: 'p1' })
  await credentials.set('m1', 'fresh')
  expect(vault.has('p1')).toBe(false)
})

test('a rejected token is dropped from machine:<id> but the profile account stays until a new token replaces both', async () => {
  const { vault, credentials } = vaultCredentials([['p1', 'dead'], ['machine:m1', 'dead']], { m1: 'p1' })
  await credentials.discard!('m1')
  expect(vault.has('machine:m1')).toBe(false)
  expect(vault.get('p1')).toBe('dead')
  expect(await credentials.get('m1')).toBeUndefined()
  await credentials.set('m1', 'fresh')
  expect(await credentials.get('m1')).toBe('fresh')
  expect(vault.get('p1')).toBe('fresh')
})

test('forgetting a machine removes both accounts', async () => {
  const { vault, credentials } = vaultCredentials([['p1', 't'], ['machine:m1', 't']], { m1: 'p1' })
  await credentials.delete('m1')
  expect(vault.size).toBe(0)
})

test('removing a migrated machine drops its profile from environments.json, other profiles stay', () => {
  writeFileSync(path.join(dir, ENVIRONMENTS_FILE), JSON.stringify(registry))
  const a = { machineId: 'env-dev', name: 'Dev server', routes: [], member: false, legacyProfileId: 'p-verified' }
  const b = { machineId: 'legacy:p-ssh', name: 'Box', routes: [], member: false, legacyProfileId: 'p-ssh' }
  writeMachineRecords(dir, [a, b])
  writeMachineRecords(dir, [b])
  const after = JSON.parse(readFileSync(path.join(dir, ENVIRONMENTS_FILE), 'utf8')) as Registry
  expect(after.environments.map((p) => p.id)).toEqual(['p-ssh'])
  // A machine added by this build has no profile; the older file is not touched for it.
  writeMachineRecords(dir, [b, { machineId: 'new', name: 'New', routes: [], member: true }])
  expect((JSON.parse(readFileSync(path.join(dir, ENVIRONMENTS_FILE), 'utf8')) as Registry).environments.map((p) => p.id)).toEqual(['p-ssh'])
})

// ADE-124: a locked Keychain is not a missing token. Treating it as one made the app mint a replacement through a
// member, which revoked the saved token on that machine, and the locked store could not keep the new one.
test('a credential store that fails is an error, not a missing token, so no replacement is minted', async () => {
  const credentials = machineCredentials({
    read: async () => { throw new Error('Unlock the system credential store or pair this environment again (Keychain -25308)') },
    write: async () => {}, remove: async () => {},
    legacyProfileOf: () => 'p1',
  })
  await expect(credentials.get('m1')).rejects.toThrow('Keychain -25308')
  const missing = machineCredentials({ read: async () => { throw new CredentialMissingError('missing') }, write: async () => {}, remove: async () => {}, legacyProfileOf: () => undefined })
  expect(await missing.get('m1')).toBeUndefined()
})

test('a file the store could not read is an error too; only CredentialMissingError means no token', async () => {
  const unreadable = machineCredentials({
    read: async () => { throw Object.assign(new Error('ENOENT: no such file or directory'), { code: 'ENOENT' }) },
    write: async () => {}, remove: async () => {},
    legacyProfileOf: () => 'p1',
  })
  await expect(unreadable.get('m1')).rejects.toThrow('ENOENT')
})

test('the scratch credentials folder reports a token it does not have as missing', async () => {
  const saved = { dir: process.env.KYBERN_GPUIX_CREDENTIALS_DIR, config: process.env.KYBERN_CLIENT_CONFIG_DIR }
  process.env.KYBERN_GPUIX_CREDENTIALS_DIR = path.join(dir, 'creds')
  process.env.KYBERN_CLIENT_CONFIG_DIR = dir
  try {
    const store = credentialStore({ platform: process.platform, env: { ...process.env }, home: dir })
    await expect(store.read('machine:none')).rejects.toBeInstanceOf(CredentialMissingError)
    const credentials = machineCredentials({ read: store.read, write: store.write, remove: store.remove, legacyProfileOf: () => undefined })
    expect(await credentials.get('none')).toBeUndefined()
    await credentials.set('m1', 'tok')
    expect(await credentials.get('m1')).toBe('tok')
  } finally {
    for (const [key, value] of [['KYBERN_GPUIX_CREDENTIALS_DIR', saved.dir], ['KYBERN_CLIENT_CONFIG_DIR', saved.config]] as const) {
      if (value === undefined) delete process.env[key]
      else process.env[key] = value
    }
  }
})
