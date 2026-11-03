// Backend choice, the `kybern credentials` contract (arguments, exit codes, no secret on the command line, no plaintext
// fallback for a locked keyring) and the file store. The platform stores themselves (Keychain, Credential Manager, Secret
// Service) run only on their own OS; the command is replaced by a fake runner here, and checked against the real Keychain by
// the manual round trip in the change notes.
import { afterEach, beforeEach, expect, test } from 'bun:test'
import { createHash } from 'node:crypto'
import { existsSync, mkdtempSync, readFileSync, rmSync, statSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import type { OsContext } from '../platform/os.ts'
import { KEYRING_EXIT, UNLOCK_KEYRING, clearCredentialCache, keychainHelper, setKeychainRunner, credentialBackends, credentialFile, credentialStore, keyringArgs, keyringOutcome, readFileToken, removeFileToken, setKeyringRunner, writeFileToken, type KeyringRun, type KeyringRunner } from './credentials.ts'

const ctx = (platform: NodeJS.Platform, env: Record<string, string> = {}): OsContext => ({ platform, env, home: platform === 'win32' ? 'C:\\Users\\ada' : '/home/ada' })
const SERVICE = 'dev.kybern.desktop.environments'

test('each platform tries its own store first; Windows and Linux keep a file only for "no store at all"', () => {
  expect(credentialBackends(ctx('darwin'))).toEqual(['keychain'])
  expect(credentialBackends(ctx('win32'))).toEqual(['keyring', 'file'])
  expect(credentialBackends(ctx('linux'))).toEqual(['keyring', 'file'])
})
test('a scratch credentials folder replaces the platform store everywhere', () => {
  for (const platform of ['darwin', 'win32', 'linux'] as const) expect(credentialBackends(ctx(platform, { KYBERN_GPUIX_CREDENTIALS_DIR: '/scratch/creds' }))).toEqual(['scratch-file'])
})
test('the file fallback lives in the per-machine data folder: %LOCALAPPDATA% on Windows, never the roaming profile', () => {
  expect(credentialFile('env 1', ctx('linux'))).toBe('/home/ada/.config/kybern-gpuix/credentials/env%201.token')
  expect(credentialFile('e', ctx('linux', { XDG_CONFIG_HOME: '/cfg' }))).toBe('/cfg/kybern-gpuix/credentials/e.token')
  expect(credentialFile('e', ctx('win32', { APPDATA: 'C:\\Users\\ada\\AppData\\Roaming', LOCALAPPDATA: 'C:\\Users\\ada\\AppData\\Local' }))).toBe('C:\\Users\\ada\\AppData\\Local\\kybern-gpuix\\credentials\\e.token')
  expect(credentialFile('e', ctx('win32'))).toBe('C:\\Users\\ada\\AppData\\Local\\kybern-gpuix\\credentials\\e.token')
  expect(credentialFile('e', ctx('linux', { KYBERN_GPUIX_CREDENTIALS_DIR: '/scratch/creds' }))).toBe('/scratch/creds/e.token')
})

test('the command line names the service and account, and never the secret', () => {
  expect(keyringArgs('get', 'abc', SERVICE)).toEqual(['credentials', 'get', '--service', SERVICE, '--account', 'abc'])
  expect(keyringArgs('set', 'abc', SERVICE)).toEqual(['credentials', 'set', '--service', SERVICE, '--account', 'abc'])
  expect(keyringArgs('delete', 'abc', SERVICE)).toEqual(['credentials', 'delete', '--service', SERVICE, '--account', 'abc'])
})
test('exit codes: 3 missing, 5 no store (fallback allowed), 4 locked (an error), anything else an error', () => {
  expect(keyringOutcome({ status: 0, stdout: 'tok', stderr: '' }, 'get')).toEqual({ kind: 'ok', value: 'tok' })
  expect(keyringOutcome({ status: 0, stdout: '', stderr: '' }, 'set')).toEqual({ kind: 'ok', value: undefined })
  expect(keyringOutcome({ status: KEYRING_EXIT.notFound, stdout: '', stderr: '' }, 'get')).toEqual({ kind: 'missing' })
  expect(keyringOutcome({ status: KEYRING_EXIT.unavailable, stdout: '', stderr: 'No system credential store is running.' }, 'get')).toMatchObject({ kind: 'unavailable' })
  expect(() => keyringOutcome({ status: KEYRING_EXIT.locked, stdout: '', stderr: '' }, 'get')).toThrow(UNLOCK_KEYRING)
  expect(() => keyringOutcome({ status: 1, stdout: 'leaked-secret', stderr: 'The system credential store failed: boom\nmore' }, 'get')).toThrow('boom')
  expect(() => keyringOutcome({ status: 1, stdout: 'leaked-secret', stderr: '' }, 'get')).not.toThrow(/leaked-secret/)
  expect(() => keyringOutcome(null, 'get')).toThrow('missing its kybern command')
})

// ---- the flow, with a fake `kybern credentials` ----

let root = '', previous: KeyringRunner
let calls: { args: string[]; input?: string }[] = []
let vault = new Map<string, string>()
/** A fake command: the vault is the keyring, `mode` is what the platform answers. */
function fake(mode: 'ok' | 'locked' | 'unavailable' | 'absent'): KeyringRunner {
  return async (args, input) => {
    calls.push({ args, input })
    if (mode === 'absent') return null
    if (mode === 'locked') return { status: KEYRING_EXIT.locked, stdout: '', stderr: '' }
    if (mode === 'unavailable') return { status: KEYRING_EXIT.unavailable, stdout: '', stderr: 'No system credential store is running.' }
    const account = args[args.indexOf('--account') + 1]!
    const run = (status: number, stdout = ''): KeyringRun => ({ status, stdout, stderr: '' })
    switch (args[1]) {
      case 'get': return vault.has(account) ? run(0, vault.get(account)!) : run(KEYRING_EXIT.notFound)
      case 'set': vault.set(account, input!); return run(0)
      default: return vault.delete(account) ? run(0) : run(KEYRING_EXIT.notFound)
    }
  }
}
const linux = () => ctx('linux', { XDG_CONFIG_HOME: path.join(root, 'cfg') })
beforeEach(() => { root = mkdtempSync(path.join(tmpdir(), 'kybern-cred-')); calls = []; vault = new Map(); clearCredentialCache(); previous = setKeyringRunner(fake('ok')) })
afterEach(() => { setKeyringRunner(previous); rmSync(root, { recursive: true, force: true }) })

test('a saved token goes to the keyring on stdin, is cached, and leaves no file behind', async () => {
  const store = credentialStore(linux())
  await store.write('env-1', 'secret-token')
  expect(vault.get('env-1')).toBe('secret-token')
  expect(calls[0]!.input).toBe('secret-token')
  expect(calls.every(call => !call.args.join(' ').includes('secret-token'))).toBe(true)
  expect(await store.read('env-1')).toBe('secret-token')
  expect(calls).toHaveLength(1) // the read came from memory
  expect(existsSync(credentialFile('env-1', linux()))).toBe(false)
})
test('a locked or dismissed keyring is an error, never a plaintext file', async () => {
  setKeyringRunner(fake('locked'))
  const store = credentialStore(linux())
  await expect(store.write('env-1', 'secret-token')).rejects.toThrow(UNLOCK_KEYRING)
  await expect(store.read('env-1')).rejects.toThrow(UNLOCK_KEYRING)
  expect(existsSync(credentialFile('env-1', linux()))).toBe(false)
  expect(existsSync(path.join(root, 'cfg'))).toBe(false)
})
test('with no Secret Service or D-Bus at all the token goes to a 0600 file, and reads come back from it', async () => {
  setKeyringRunner(fake('unavailable'))
  const store = credentialStore(linux())
  await store.write('env-1', 'secret-token')
  const file = credentialFile('env-1', linux())
  expect(readFileToken(file)).toBe('secret-token')
  if (process.platform !== 'win32') expect(statSync(file).mode & 0o777).toBe(0o600)
  clearCredentialCache()
  expect(await store.read('env-1')).toBe('secret-token')
  await store.remove('env-1')
  expect(existsSync(file)).toBe(false)
})
test('a token in the keyring replaces a stale file, and a missing keyring entry falls back to a file saved earlier', async () => {
  const file = credentialFile('env-1', linux())
  writeFileToken(file, 'old-file-token')
  const store = credentialStore(linux())
  expect(await store.read('env-1')).toBe('old-file-token') // keyring: not found, file: found
  clearCredentialCache()
  await store.write('env-1', 'new-token')
  expect(existsSync(file)).toBe(false)
  clearCredentialCache()
  expect(await store.read('env-1')).toBe('new-token')
})
test('without the bundled command there is an error to act on, not a silent file', async () => {
  setKeyringRunner(fake('absent'))
  const store = credentialStore(linux())
  await expect(store.write('env-1', 'secret-token')).rejects.toThrow('Reinstall Kybern')
  expect(existsSync(credentialFile('env-1', linux()))).toBe(false)
})
test('the command runs asynchronously: the caller gets a promise and the event loop keeps turning while it waits', async () => {
  let release!: () => void
  setKeyringRunner(() => new Promise(resolve => { release = () => resolve({ status: 0, stdout: 'late', stderr: '' }) }))
  const pending = credentialStore(linux()).read('env-1')
  let turned = false
  setTimeout(() => { turned = true }, 0)
  await new Promise(resolve => setTimeout(resolve, 5))
  expect(turned).toBe(true)
  release()
  expect(await pending).toBe('late')
})
test('Windows keeps its fallback file under %LOCALAPPDATA%, and tries the keyring first', () => {
  const win = ctx('win32', { APPDATA: 'D:\\Roaming', LOCALAPPDATA: 'D:\\Local' })
  expect(credentialFile('e', win)).toBe('D:\\Local\\kybern-gpuix\\credentials\\e.token')
  expect(credentialBackends(win)).toEqual(['keyring', 'file'])
})

test('the file store keeps the token private: 0600 file in a 0700 folder, replaced atomically, removed quietly', () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'kybern-cred-file-'))
  try {
    const file = path.join(dir, 'creds', 'e.token')
    expect(readFileToken(file)).toBeNull()
    writeFileToken(file, 'secret-1')
    writeFileToken(file, 'secret-2')
    expect(readFileToken(file)).toBe('secret-2')
    if (process.platform !== 'win32') {
      expect(statSync(file).mode & 0o777).toBe(0o600)
      expect(statSync(path.dirname(file)).mode & 0o777).toBe(0o700)
    }
    removeFileToken(file)
    removeFileToken(file)
    expect(readFileToken(file)).toBeNull()
  } finally { rmSync(dir, { recursive: true, force: true }) }
})

// ---- macOS: one combined item through the kybern-keychain helper ----

test('macOS keeps every token in one item through the helper, which gets the same arguments as `kybern credentials`', async () => {
  const runs: { args: string[]; input?: string }[] = []
  const previousHelper = setKeychainRunner(async (args, input) => {
    runs.push({ args, input })
    const account = args[args.indexOf('--account') + 1]!
    switch (args[1]) {
      case 'get': return vault.has(account) ? { status: 0, stdout: vault.get(account)!, stderr: '' } : { status: KEYRING_EXIT.notFound, stdout: '', stderr: '' }
      case 'set': vault.set(account, input!); return { status: 0, stdout: '', stderr: '' }
      default: return { status: vault.delete(account) ? 0 : KEYRING_EXIT.notFound, stdout: '', stderr: '' }
    }
  })
  try {
    const mac = ctx('darwin', { KYBERN_CLIENT_CONFIG_DIR: path.join(root, 'cfg'), KYBERN_GPUIX_KEYCHAIN_SERVICE: 'kybern-gpuix-test-unit' })
    vault.set('machine:old', 'old-token') // what 0.21 left behind
    const store = credentialStore(mac)
    await store.write('machine:new', 'new-token')
    expect(await store.read('machine:old')).toBe('old-token')
    expect([...vault.keys()]).toEqual(['all'])
    expect(JSON.parse(vault.get('all')!)).toEqual({ 'machine:new': 'new-token', 'machine:old': 'old-token' })
    expect(runs[0]!.args).toEqual(keyringArgs('get', 'all', 'kybern-gpuix-test-unit'))
    expect(runs.every(run => !run.args.join(' ').includes('token'))).toBe(true)
    await store.remove('machine:new')
    expect(JSON.parse(vault.get('all')!)).toEqual({ 'machine:old': 'old-token' })
    await expect(store.read('machine:new')).rejects.toThrow('pair this environment again')
    // The lock file is gone once each change is done.
    expect(existsSync(path.join(root, 'cfg', 'kybern-gpuix-test-unit.lock'))).toBe(false)
    setKeychainRunner(async () => null)
    clearCredentialCache()
    await expect(credentialStore(mac).read('machine:old')).rejects.toThrow('kybern-keychain helper')
  } finally { setKeychainRunner(previousHelper) }
})

test('the committed helper is the pinned build: a new one asks every user again (packaging/keychain/README.md)', () => {
  const dir = path.join(import.meta.dir, '../../packaging/keychain')
  const pin = JSON.parse(readFileSync(path.join(dir, 'pin.json'), 'utf8')) as { sha256: string }
  expect(createHash('sha256').update(readFileSync(path.join(dir, 'kybern-keychain'))).digest('hex')).toBe(pin.sha256)
  // From source, the app runs the committed helper; packaged, the copy next to its executable.
  expect(keychainHelper('/nowhere/bun')).toBe(path.join(dir, 'kybern-keychain'))
})
