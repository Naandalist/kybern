// The combined Keychain item and the move from one item per id, against a fake Keychain. The real one is exercised by
// the packaged-app check in packaging/keychain/README.md.
import { expect, test } from 'bun:test'
import { COMBINED_ACCOUNT, COMBINED_FRESH_MS, combinedKeychain, parseCombined, type KeychainItems, type Lock } from './combinedKeychain.ts'

/** A fake Keychain: `denied` items throw like a declined prompt; every call is recorded. */
function fakeKeychain(initial: Record<string, string> = {}) {
  const items = new Map(Object.entries(initial))
  const denied = new Set<string>()
  const calls: string[] = []
  const api: KeychainItems = {
    async get(account) { calls.push(`get ${account}`); if (denied.has(account)) throw new Error('Unlock your keyring, then try again.'); return items.get(account) ?? null },
    async set(account, value) { calls.push(`set ${account}`); items.set(account, value) },
    async delete(account) { calls.push(`delete ${account}`); items.delete(account) },
  }
  return { api, items, denied, calls, combined: () => JSON.parse(items.get(COMBINED_ACCOUNT) ?? '{}') as Record<string, string> }
}
/** A lock that serializes like the lock file does. */
function serialLock(): Lock {
  let tail: Promise<unknown> = Promise.resolve()
  return work => { const run = tail.then(work); tail = run.catch(() => {}); return run }
}

test('a token is written into the one combined item and read back from it', async () => {
  const kc = fakeKeychain()
  const store = combinedKeychain(kc.api, serialLock())
  await store.write('machine:a', 'tok-a')
  await store.write('machine:b', 'tok-b')
  expect(kc.combined()).toEqual({ 'machine:a': 'tok-a', 'machine:b': 'tok-b' })
  expect([...kc.items.keys()]).toEqual([COMBINED_ACCOUNT])
  expect(await store.read('machine:a')).toBe('tok-a')
  expect(await store.read('machine:none')).toBeNull()
})

test('an old per-id item is merged into the combined map, then deleted; the other entries stay', async () => {
  const kc = fakeKeychain({ [COMBINED_ACCOUNT]: JSON.stringify({ 'machine:a': 'tok-a' }), 'machine:b': 'old-b', 'machine:c': 'old-c' })
  const store = combinedKeychain(kc.api, serialLock())
  expect(await store.read('machine:b')).toBe('old-b')
  expect(kc.combined()).toEqual({ 'machine:a': 'tok-a', 'machine:b': 'old-b' })
  expect(kc.items.has('machine:b')).toBe(false)
  expect(kc.items.get('machine:c')).toBe('old-c') // not read yet: untouched
  // The old item is deleted only after the combined write.
  expect(kc.calls.indexOf(`set ${COMBINED_ACCOUNT}`)).toBeLessThan(kc.calls.indexOf('delete machine:b'))
})

test('the first run with N old items and no combined item ends with one item holding all N', async () => {
  const old = { 'machine:1': 't1', 'machine:2': 't2', 'machine:3': 't3', 'machine:4': 't4' }
  const kc = fakeKeychain(old)
  const store = combinedKeychain(kc.api, serialLock())
  const read = await Promise.all(Object.keys(old).map(id => store.read(id)))
  expect(read).toEqual(['t1', 't2', 't3', 't4'])
  expect(kc.combined()).toEqual(old)
  expect([...kc.items.keys()]).toEqual([COMBINED_ACCOUNT])
  // Each old item was read exactly once: one prompt per item on that first run, never two.
  for (const id of Object.keys(old)) expect(kc.calls.filter(call => call === `get ${id}`)).toHaveLength(1)
})

test('a declined or failed old item is kept and moved on a later launch; the rest still move', async () => {
  const kc = fakeKeychain({ 'machine:1': 't1', 'machine:2': 't2' })
  kc.denied.add('machine:2')
  const first = combinedKeychain(kc.api, serialLock())
  expect(await first.read('machine:1')).toBe('t1')
  await expect(first.read('machine:2')).rejects.toThrow('Unlock')
  expect(kc.items.get('machine:2')).toBe('t2')
  expect(kc.combined()).toEqual({ 'machine:1': 't1' })
  // Next launch, the user allows it.
  kc.denied.clear()
  const second = combinedKeychain(kc.api, serialLock())
  expect(await second.read('machine:2')).toBe('t2')
  expect(kc.combined()).toEqual({ 'machine:1': 't1', 'machine:2': 't2' })
  expect([...kc.items.keys()]).toEqual([COMBINED_ACCOUNT])
})

test('a failed combined write keeps the old item', async () => {
  const kc = fakeKeychain({ 'machine:1': 't1' })
  const store = combinedKeychain({ ...kc.api, set: async () => { throw new Error('Keychain -61') } }, serialLock())
  await expect(store.read('machine:1')).rejects.toThrow('-61')
  expect(kc.items.get('machine:1')).toBe('t1')
})

test('moving is idempotent: a second run, or a second read, touches no old item', async () => {
  const kc = fakeKeychain({ 'machine:1': 't1' })
  const store = combinedKeychain(kc.api, serialLock())
  expect(await store.read('machine:1')).toBe('t1')
  const writes = kc.calls.filter(call => call.startsWith('set')).length
  const again = combinedKeychain(kc.api, serialLock())
  expect(await again.read('machine:1')).toBe('t1')
  expect(kc.calls.filter(call => call.startsWith('set')).length).toBe(writes)
  expect(kc.calls.filter(call => call === 'get machine:1')).toHaveLength(1)
  // Saving the same token again changes nothing either.
  await again.write('machine:1', 't1')
  expect(kc.calls.filter(call => call.startsWith('set')).length).toBe(writes)
})

test('a token another window saved meanwhile wins over an old item', async () => {
  const kc = fakeKeychain({ 'machine:1': 'old' })
  const store = combinedKeychain({ ...kc.api, get: async account => {
    const value = await kc.api.get(account)
    // Another process pairs again between our read of the old item and our write.
    if (account === 'machine:1') kc.items.set(COMBINED_ACCOUNT, JSON.stringify({ 'machine:1': 'newer' }))
    return value
  } }, serialLock())
  expect(await store.read('machine:1')).toBe('newer')
  expect(kc.combined()).toEqual({ 'machine:1': 'newer' })
  expect(kc.items.has('machine:1')).toBe(false)
})

test('two windows moving the same old items at once read each once, and neither finds it missing', async () => {
  const old = { 'machine:1': 't1', 'machine:2': 't2' }
  const kc = fakeKeychain(old)
  const lock = serialLock()
  const windowA = combinedKeychain(kc.api, lock), windowB = combinedKeychain(kc.api, lock)
  const [a, b] = await Promise.all([Promise.all(Object.keys(old).map(id => windowA.read(id))), Promise.all(Object.keys(old).map(id => windowB.read(id)))])
  expect(a).toEqual(['t1', 't2'])
  expect(b).toEqual(['t1', 't2'])
  for (const id of Object.keys(old)) expect(kc.calls.filter(call => call === `get ${id}`)).toHaveLength(1)
  expect(kc.combined()).toEqual(old)
  expect([...kc.items.keys()]).toEqual([COMBINED_ACCOUNT])
})

test('a window that read an empty map before another window moved the item still finds it', async () => {
  const kc = fakeKeychain({ 'machine:1': 't1' })
  const lock = serialLock()
  const windowA = combinedKeychain(kc.api, lock), windowB = combinedKeychain(kc.api, lock)
  await windowB.read('other') // B's map is cached and lacks machine:1
  expect(await windowA.read('machine:1')).toBe('t1') // A moves it and deletes the old item
  expect(await windowB.read('machine:1')).toBe('t1')
  expect(kc.calls.filter(call => call === 'get machine:1')).toHaveLength(1)
})

test('changes are read-modify-write from the item itself, so two windows never drop each other\'s tokens', async () => {
  const kc = fakeKeychain()
  const lock = serialLock()
  const windowA = combinedKeychain(kc.api, lock), windowB = combinedKeychain(kc.api, lock)
  await windowA.read('x') // A now holds an (empty) map in memory
  await windowB.write('machine:b', 'tok-b')
  await windowA.write('machine:a', 'tok-a')
  expect(kc.combined()).toEqual({ 'machine:a': 'tok-a', 'machine:b': 'tok-b' })
})

test('reads in a burst share one fetch; after a moment the item is fetched again', async () => {
  let clock = 0
  const kc = fakeKeychain({ [COMBINED_ACCOUNT]: JSON.stringify({ a: '1', b: '2' }) })
  const store = combinedKeychain(kc.api, serialLock(), () => clock)
  await Promise.all([store.read('a'), store.read('b'), store.read('a')])
  expect(kc.calls.filter(call => call === `get ${COMBINED_ACCOUNT}`)).toHaveLength(1)
  kc.items.set(COMBINED_ACCOUNT, JSON.stringify({ a: 'repaired', b: '2' }))
  clock += COMBINED_FRESH_MS
  expect(await store.read('a')).toBe('repaired')
})

test('remove clears the entry and any old item of that id', async () => {
  const kc = fakeKeychain({ [COMBINED_ACCOUNT]: JSON.stringify({ a: '1', b: '2' }), a: 'old-a' })
  const store = combinedKeychain(kc.api, serialLock())
  await store.remove('a')
  expect(kc.combined()).toEqual({ b: '2' })
  expect(kc.items.has('a')).toBe(false)
  await store.remove('missing') // nothing to do, no error
})

test('a damaged combined item is an error, never overwritten', async () => {
  expect(parseCombined(null)).toEqual({})
  expect(() => parseCombined('{"a": 1}')).toThrow('damaged')
  expect(() => parseCombined('[]')).toThrow('damaged')
  const kc = fakeKeychain({ [COMBINED_ACCOUNT]: 'not json' })
  const store = combinedKeychain(kc.api, serialLock())
  await expect(store.write('a', '1')).rejects.toThrow('damaged')
  expect(kc.items.get(COMBINED_ACCOUNT)).toBe('not json')
})
