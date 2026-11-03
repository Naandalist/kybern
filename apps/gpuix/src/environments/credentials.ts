// The same generic-password service/account as the Tauri client (its `keyring` crate), on each platform's own store. Secrets
// never enter environments.json, command arguments, logs, or UI persistence.
//
// - macOS: the Keychain, through the bundled `kybern-keychain` helper (packaging/keychain), never in process. The login
//   keychain lets only the creating program's exact build (its cdhash) read an item without asking for the keychain
//   password, and the app's executable changes with every release; the helper's bytes do not. All tokens live in one
//   item (`combinedKeychain.ts`), so a new helper would cost one prompt, not one per environment.
// - Windows and Linux: the bundled `kybern credentials get|set|delete` command (crates/kybern-cli/src/credentials.rs), which
//   is the `keyring` crate the Tauri client uses, so both clients share one set of entries on Credential Manager and the
//   Secret Service. It runs asynchronously (an unlock prompt can take a while), the secret travels on stdin and stdout, and
//   each token is cached in memory per id after its first read or write.
// - A locked or dismissed keyring is an error: never a reason to write the token to a plain file. Only a machine with no
//   Secret Service or D-Bus at all (the command exits 5) stores it in a 0600 file under the GPUIX data folder instead.
// - A scratch run (`KYBERN_GPUIX_CREDENTIALS_DIR`) uses only that folder, on every platform.
import { spawn } from 'node:child_process'
import { randomUUID } from 'node:crypto'
import { closeSync, existsSync, mkdirSync, openSync, readFileSync, renameSync, statSync, unlinkSync, utimesSync, writeFileSync, writeSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { combinedKeychain, type CombinedKeychain, type KeychainItems } from './combinedKeychain.ts'
import { gpuixLocalDir, osContext, osName, pathFor, type OsContext } from '../platform/os.ts'
import { bundledCommand } from '../shell/daemon.ts'

const DESKTOP_SERVICE = 'dev.kybern.desktop.environments'
/** Checks exercise the real store under a scratch service (`KYBERN_GPUIX_KEYCHAIN_SERVICE=kybern-gpuix-test-…`, with a scratch client config), never the desktop's items. */
function serviceName(ctx: OsContext = osContext()): string {
  const scratch = ctx.env.KYBERN_GPUIX_KEYCHAIN_SERVICE
  if (!scratch) return DESKTOP_SERVICE
  if (!ctx.env.KYBERN_CLIENT_CONFIG_DIR || !/^kybern-gpuix-test-[\w-]+$/.test(scratch)) throw new Error('A scratch Keychain service needs KYBERN_CLIENT_CONFIG_DIR and a kybern-gpuix-test-… name')
  return scratch
}

export type CredentialBackend = 'scratch-file' | 'keychain' | 'keyring' | 'file'
/** The stores to try, best first. A write goes to the first that works; a read takes the first that has the token; a remove clears all. */
export function credentialBackends(ctx: OsContext = osContext()): CredentialBackend[] {
  if (ctx.env.KYBERN_GPUIX_CREDENTIALS_DIR) return ['scratch-file']
  switch (osName(ctx.platform)) {
    case 'macos': return ['keychain']
    case 'windows': return ['keyring', 'file']
    case 'linux': return ['keyring', 'file']
  }
}
/** Where a token lives when no credential store runs at all: GPUIX's per-machine data folder (`%LOCALAPPDATA%` on Windows), readable by the user alone. */
export const credentialFile = (id: string, ctx: OsContext = osContext()) => pathFor(ctx.platform).join(ctx.env.KYBERN_GPUIX_CREDENTIALS_DIR ?? pathFor(ctx.platform).join(gpuixLocalDir(ctx), 'credentials'), `${encodeURIComponent(id)}.token`)
function scratchFile(id: string) {
  if (!process.env.KYBERN_CLIENT_CONFIG_DIR) throw new Error('Isolated credentials require KYBERN_CLIENT_CONFIG_DIR')
  return credentialFile(id)
}
export function readFileToken(file: string): string | null {
  try { return readFileSync(file, 'utf8').trim() || null } catch (e) { if ((e as NodeJS.ErrnoException).code === 'ENOENT') return null; throw e }
}
export function writeFileToken(file: string, token: string) {
  mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 })
  const temp = `${file}.${process.pid}.tmp`
  writeFileSync(temp, token, { mode: 0o600 }); renameSync(temp, file)
}
export function removeFileToken(file: string) {
  try { unlinkSync(file) } catch (e) { if ((e as NodeJS.ErrnoException).code !== 'ENOENT') throw e }
}

// ---- Windows and Linux: the bundled `kybern credentials` command ----

/** Exit codes of `kybern credentials` (crates/kybern-cli/src/credentials.rs). */
export const KEYRING_EXIT = { notFound: 3, locked: 4, unavailable: 5 } as const
export interface KeyringRun { status: number | null; stdout: string; stderr: string }
/** Runs `kybern credentials …`. `null` when the command is not installed. Replaced in tests. */
export type KeyringRunner = (args: string[], input?: string) => Promise<KeyringRun | null>

/** `kybern credentials <op>` arguments. The secret is never one of them: `set` reads it from stdin, `get` prints it on stdout. */
export function keyringArgs(op: 'get' | 'set' | 'delete', id: string, serviceId: string): string[] {
  return ['credentials', op, '--service', serviceId, '--account', id]
}
const KEYRING_TIMEOUT_MS = 120_000
/** Runs `command …args`, the secret on stdin; `null` when the command is missing. */
const runCommand = (command: string | undefined, args: string[], input?: string) => new Promise<KeyringRun | null>((resolve, reject) => {
  if (!command) return resolve(null)
  let child
  try { child = spawn(command, args, { stdio: [input === undefined ? 'ignore' : 'pipe', 'pipe', 'pipe'], windowsHide: true }) } catch (error) { return reject(error) }
  let stdout = '', stderr = '', done = false
  const finish = (run: KeyringRun | Error) => { if (done) return; done = true; clearTimeout(timer); run instanceof Error ? reject(run) : resolve(run) }
  // A dismissed unlock prompt can leave the command waiting; stop it rather than hang the connection.
  const timer = setTimeout(() => { child.kill(); finish(new Error('The system credential store did not answer. Unlock your keyring, then try again.')) }, KEYRING_TIMEOUT_MS)
  child.stdout!.on('data', chunk => { stdout += chunk })
  child.stderr!.on('data', chunk => { stderr += chunk })
  child.on('error', error => finish(error))
  child.on('close', status => finish({ status, stdout, stderr }))
  if (input !== undefined) { child.stdin!.on('error', () => {}); child.stdin!.end(input) }
})
const runKeyringCommand: KeyringRunner = (args, input) => runCommand(bundledCommand('kybern'), args, input)
let keyringRunner: KeyringRunner = runKeyringCommand
/** Replace the command runner (tests); returns the previous one. */
export function setKeyringRunner(runner: KeyringRunner | null): KeyringRunner {
  const previous = keyringRunner
  keyringRunner = runner ?? runKeyringCommand
  return previous
}

// ---- macOS: the `kybern-keychain` helper ----

/** The helper next to the app's executable (`Contents/MacOS`), or the committed one when running from source. */
export function keychainHelper(execPath = process.execPath): string | undefined {
  const candidates = [path.join(path.dirname(execPath), 'kybern-keychain'), fileURLToPath(new URL('../../packaging/keychain/kybern-keychain', import.meta.url))]
  return candidates.find(candidate => existsSync(candidate) && statSync(candidate).isFile())
}
const runKeychainHelper: KeyringRunner = (args, input) => runCommand(keychainHelper(), args, input)
let keychainRunner: KeyringRunner = runKeychainHelper
/** Replace the helper runner (tests); returns the previous one. */
export function setKeychainRunner(runner: KeyringRunner | null): KeyringRunner {
  const previous = keychainRunner
  keychainRunner = runner ?? runKeychainHelper
  return previous
}
/** One service's raw items through the helper; it speaks the `kybern credentials` contract. */
export function helperItems(serviceId: string, run: () => KeyringRunner = () => keychainRunner): KeychainItems {
  const call = async (op: 'get' | 'set' | 'delete', account: string, input?: string) => {
    const result = await run()(keyringArgs(op, account, serviceId), input)
    if (!result) throw new Error('This install of Kybern is missing its kybern-keychain helper, which holds your saved device tokens. Reinstall Kybern, then try again.')
    return keyringOutcome(result, op)
  }
  return {
    get: async account => { const outcome = await call('get', account); return outcome.kind === 'ok' ? outcome.value! : null },
    set: async (account, value) => { await call('set', account, value) },
    delete: async account => { await call('delete', account) },
  }
}
export interface FileLockTiming {
  /** How long a caller waits for the lock: longer than one Keychain dialog in the window that holds it. */
  waitMs: number
  /** A lock whose file was not touched for this long belongs to a process that stopped; it is taken over. */
  staleMs: number
  /** How often the holder touches the file while its work runs (a Keychain dialog can take up to two minutes). */
  heartbeatMs: number
}
export const LOCK_TIMING: FileLockTiming = { waitMs: KEYRING_TIMEOUT_MS + 15_000, staleMs: 30_000, heartbeatMs: 2_000 }

const lockOwner = (file: string) => { try { return readFileSync(file, 'utf8') } catch { return null } }
/** Whether the process named in a lock (`<pid>:<nonce>`) still runs; a lock without one (older releases) counts as alive. */
function ownerRuns(owner: string): boolean {
  const pid = Number(owner.split(':')[0])
  if (!Number.isInteger(pid) || pid <= 0) return true
  try { process.kill(pid, 0); return true } catch (error) { return (error as NodeJS.ErrnoException).code === 'EPERM' }
}
/** Remove the lock at `file` if its holder is gone or stopped touching it. True when the caller should try again. */
function breakStaleLock(file: string, staleMs: number): boolean {
  const owner = lockOwner(file)
  let touched: number
  try { touched = statSync(file).mtimeMs } catch { return true } // released meanwhile
  if (owner === null) return true
  const gone = owner !== '' && !ownerRuns(owner)
  if (!gone && Date.now() - touched <= staleMs) return false
  // Only the lock judged stale goes, never one another window took a moment ago.
  if (lockOwner(file) !== owner) return true
  try { unlinkSync(file) } catch { /* another waiter removed it */ }
  return true
}

/**
 * A lock file shared by every GPUIX process of this user. The file names its holder (`<pid>:<nonce>`), which touches it
 * while its work runs, so a Keychain dialog that keeps the lock for a while does not let another window take it; a lock
 * whose holder exited or stopped touching it is taken over. Only the holder removes its own lock.
 */
export function fileLock(file: string, timing: FileLockTiming = LOCK_TIMING) {
  return async <T>(work: () => Promise<T>): Promise<T> => {
    mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 })
    const me = `${process.pid}:${randomUUID()}`
    const deadline = Date.now() + timing.waitMs
    for (;;) {
      try {
        const fd = openSync(file, 'wx', 0o600)
        try { writeSync(fd, me) } finally { closeSync(fd) }
        break
      } catch (error) {
        if ((error as NodeJS.ErrnoException).code !== 'EEXIST') throw error
        if (breakStaleLock(file, timing.staleMs)) continue
        if (Date.now() > deadline) throw new Error('Another Kybern window is saving a device token. Try again in a moment.')
        await new Promise(resolve => setTimeout(resolve, 25))
      }
    }
    const heartbeat = setInterval(() => {
      if (lockOwner(file) !== me) return
      try { const at = new Date(); utimesSync(file, at, at) } catch { /* released */ }
    }, timing.heartbeatMs)
    heartbeat.unref?.()
    try { return await work() } finally {
      clearInterval(heartbeat)
      if (lockOwner(file) === me) try { unlinkSync(file) } catch { /* already gone */ }
    }
  }
}
const combined = new Map<string, CombinedKeychain>()
function macKeychain(ctx: OsContext) {
  const serviceId = serviceName(ctx)
  let store = combined.get(serviceId)
  if (!store) combined.set(serviceId, store = combinedKeychain(helperItems(serviceId), fileLock(pathFor(ctx.platform).join(gpuixLocalDir(ctx), `${serviceId}.lock`))))
  return store
}
// ---- one store at a time ----

type Outcome = { kind: 'ok'; value?: string } | { kind: 'missing'; detail?: string } | { kind: 'unavailable'; detail?: string }
interface Store {
  read(id: string, ctx: OsContext): Promise<Outcome>
  write(id: string, token: string, ctx: OsContext): Promise<Outcome>
  remove(id: string, ctx: OsContext): Promise<Outcome>
}
export const UNLOCK_KEYRING = 'Unlock your keyring, then try again.'
/** No store holds a token under this id. Anything else a read throws is the store failing (locked, dismissed, broken). */
export class CredentialMissingError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'CredentialMissingError'
  }
}
const UNLOCK = 'Unlock the system credential store or pair this environment again'
const ok = (value?: string): Outcome => ({ kind: 'ok', value })

const keychain: Store = {
  async read(id, ctx) { const token = await macKeychain(ctx).read(id); return token === null ? { kind: 'missing' } : ok(token) },
  async write(id, token, ctx) { await macKeychain(ctx).write(id, token); return ok() },
  async remove(id, ctx) { await macKeychain(ctx).remove(id); return ok() },
}

/** What a `kybern credentials` run means: an outcome the stores can fall back on, or an error the user must act on (never echoes the input). */
export function keyringOutcome(run: KeyringRun | null, op: 'get' | 'set' | 'delete'): Outcome {
  if (!run) throw new Error('This install of Kybern is missing its kybern command, which holds your saved device tokens. Reinstall Kybern, then try again.')
  if (run.status === 0) return ok(op === 'get' ? run.stdout : undefined)
  if (run.status === KEYRING_EXIT.notFound) return { kind: 'missing' }
  if (run.status === KEYRING_EXIT.unavailable) return { kind: 'unavailable', detail: run.stderr.trim().slice(0, 160) }
  if (run.status === KEYRING_EXIT.locked) throw new Error(UNLOCK_KEYRING)
  throw new Error(`${UNLOCK} (${run.stderr.trim().split('\n')[0]?.slice(0, 160) || `kybern credentials ${run.status ?? 'did not finish'}`})`)
}
const keyring: Store = {
  read: async (id, ctx) => keyringOutcome(await keyringRunner(keyringArgs('get', id, serviceName(ctx))), 'get'),
  write: async (id, token, ctx) => keyringOutcome(await keyringRunner(keyringArgs('set', id, serviceName(ctx)), token), 'set'),
  remove: async (id, ctx) => keyringOutcome(await keyringRunner(keyringArgs('delete', id, serviceName(ctx))), 'delete'),
}

const fileStore: Store = {
  read: async (id, ctx) => { const token = readFileToken(credentialFile(id, ctx)); return token ? ok(token) : { kind: 'missing' } },
  write: async (id, token, ctx) => { writeFileToken(credentialFile(id, ctx), token); return ok() },
  remove: async (id, ctx) => { removeFileToken(credentialFile(id, ctx)); return ok() },
}
const scratchStore: Store = {
  read: async (id) => { const token = readFileToken(scratchFile(id)); return token ? ok(token) : { kind: 'missing' } },
  write: async (id, token) => { writeFileToken(scratchFile(id), token); return ok() },
  remove: async (id) => { removeFileToken(scratchFile(id)); return ok() },
}
const STORES: Record<CredentialBackend, Store> = { 'scratch-file': scratchStore, keychain, keyring, file: fileStore }

// Off macOS each read is a process spawn, so a token read or written once stays in memory for the life of the process.
const cache = new Map<string, string>()
/** Forget every cached token (tests). */
export function clearCredentialCache() { cache.clear(); combined.clear() }

let warnedFile = false
/** The credential store of `ctx` (this machine by default). `readCredential` and the others are this, bound to the real process. */
export function credentialStore(ctx: OsContext = osContext()) {
  const cached = osName(ctx.platform) !== 'macos'
  return {
    async read(id: string): Promise<string> {
      const hit = cached ? cache.get(id) : undefined
      if (hit !== undefined) return hit
      let detail: string | undefined
      for (const backend of credentialBackends(ctx)) {
        const result = await STORES[backend].read(id, ctx)
        if (result.kind === 'ok') { if (cached) cache.set(id, result.value!); return result.value! }
        detail ??= result.detail
      }
      throw new CredentialMissingError(detail ? `${UNLOCK} (${detail})` : UNLOCK)
    },
    async write(id: string, token: string): Promise<void> {
      const backends = credentialBackends(ctx)
      let detail: string | undefined
      for (const backend of backends) {
        const result = await STORES[backend].write(id, token, ctx)
        if (result.kind !== 'ok') { detail ??= result.detail; continue }
        if (backend === 'file' && backends.length > 1 && !warnedFile) { warnedFile = true; console.warn(`[credentials] no system credential store is running${detail ? ` (${detail})` : ''}; the token is kept in a private file in GPUIX's data folder`) }
        // A token saved to the file store while no keyring ran would otherwise outlive the one now in the keyring.
        if (backend !== 'file' && backend !== 'scratch-file' && backends.includes('file')) removeFileToken(credentialFile(id, ctx))
        if (cached) cache.set(id, token)
        return
      }
      throw new Error(detail ? `${UNLOCK} (${detail})` : UNLOCK)
    },
    async remove(id: string): Promise<void> {
      cache.delete(id)
      for (const backend of credentialBackends(ctx)) await STORES[backend].remove(id, ctx)
    },
  }
}
export const readCredential = (id: string): Promise<string> => credentialStore().read(id)
export const writeCredential = (id: string, token: string): Promise<void> => credentialStore().write(id, token)
export const removeCredential = (id: string): Promise<void> => credentialStore().remove(id)
