// One GPUIX window's part of the multi-process migration check (`fileLock.test.ts`): it migrates `ids` from a
// Keychain stand-in kept as files in `dir` (one file per item, every read appended to `dir/reads.log`), under the real
// lock file, and prints what it read as JSON. Reading an old item waits a while, as a Keychain dialog does.
import { appendFileSync, existsSync, readFileSync, unlinkSync, writeFileSync } from 'node:fs'
import path from 'node:path'
import { combinedKeychain, type KeychainItems } from './combinedKeychain.ts'
import { fileLock } from './credentials.ts'

const [dir, lockFile, ...ids] = process.argv.slice(2)
if (!dir || !lockFile) throw new Error('usage: keychainMigration.process.ts <dir> <lock file> <id>…')
const file = (account: string) => path.join(dir, `${encodeURIComponent(account)}.item`)
const items: KeychainItems = {
  async get(account) {
    appendFileSync(path.join(dir, 'reads.log'), `${process.pid} ${account}\n`)
    if (account !== 'all') await new Promise(resolve => setTimeout(resolve, 150))
    return existsSync(file(account)) ? readFileSync(file(account), 'utf8') : null
  },
  async set(account, value) { writeFileSync(file(account), value) },
  async delete(account) { try { unlinkSync(file(account)) } catch { /* missing */ } },
}
const store = combinedKeychain(items, fileLock(lockFile, { waitMs: 20_000, staleMs: 30_000, heartbeatMs: 50 }))
const read = await Promise.all(ids.map(id => store.read(id)))
console.log(JSON.stringify(read))
