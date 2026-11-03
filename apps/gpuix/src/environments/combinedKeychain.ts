// macOS keeps every environment token in ONE Keychain item (account `all`, a JSON map of id → token) instead of one item
// per id. Each item has its own access list, so N items meant N "wants to use your confidential information" prompts
// whenever the program reading them changed; one item means at most one.
//
// Older releases wrote one item per id (`machine:<id>`, profile ids). Each is folded in the first time its id is read:
// the old item is read (the one prompt it may cost), the token is written into the combined item, and only after that
// write succeeded is the old item deleted. A read that fails or is declined keeps the old item, so the next launch tries
// again; a token is never dropped. The move runs under the lock and looks at the combined item first, so when several
// windows open at once only one reads the old item (one prompt, not one per window) and the others find its token in
// the combined item instead of reporting it missing (which would pair again and revoke the token just moved).
//
// Several GPUIX processes (one per environment window) share the item, so every change is a read-modify-write under a
// lock that the caller provides (a lock file in production). Reads always fetch the item again unless a read in the
// last moment already did: another window may have paired since.

/** Raw Keychain items of one service. `get` is `null` for no such item; a locked or declined Keychain throws. */
export interface KeychainItems {
  get(account: string): Promise<string | null>
  set(account: string, value: string): Promise<void>
  /** Removes the item; a missing item is not an error. */
  delete(account: string): Promise<void>
}
export type Lock = <T>(work: () => Promise<T>) => Promise<T>

export const COMBINED_ACCOUNT = 'all'
/** How long a fetched map answers reads without asking the Keychain again (a burst of reads at startup → one fetch). */
export const COMBINED_FRESH_MS = 2_000

/** The combined item's text as a map. Anything else is an error: never overwrite what can't be read. */
export function parseCombined(text: string | null): Record<string, string> {
  if (text === null || text.trim() === '') return {}
  let value: unknown
  try { value = JSON.parse(text) } catch { throw new Error('Kybern\'s saved device tokens in the Keychain are damaged. Pair your environments again.') }
  if (!value || typeof value !== 'object' || Array.isArray(value) || !Object.values(value).every(token => typeof token === 'string')) throw new Error('Kybern\'s saved device tokens in the Keychain are damaged. Pair your environments again.')
  return value as Record<string, string>
}

export interface CombinedKeychain {
  /** The token, or `null` when neither the combined item nor an old per-id item has one. */
  read(id: string): Promise<string | null>
  write(id: string, token: string): Promise<void>
  remove(id: string): Promise<void>
}

export function combinedKeychain(items: KeychainItems, lock: Lock, now: () => number = Date.now): CombinedKeychain {
  let cached: { map: Record<string, string>; at: number } | undefined
  let loading: Promise<Record<string, string>> | undefined
  const migrating = new Map<string, Promise<string | null>>()

  const fetch = async () => {
    const map = parseCombined(await items.get(COMBINED_ACCOUNT))
    cached = { map, at: now() }
    return map
  }
  /** The map for a read: one fetch for concurrent callers, and none while the last fetch is fresh. */
  const current = () => {
    if (cached && now() - cached.at < COMBINED_FRESH_MS) return Promise.resolve(cached.map)
    return loading ??= fetch().finally(() => { loading = undefined })
  }
  /** Read-modify-write under the lock, always from the item itself; `change` returns false when nothing changed. */
  const update = (change: (map: Record<string, string>) => boolean) => lock(async () => {
    const map = await fetch()
    if (change(map)) await items.set(COMBINED_ACCOUNT, JSON.stringify(map))
    cached = { map, at: now() }
    return map
  })
  const dropOld = (id: string) => items.delete(id).catch(() => { /* removed on a later write or migration */ })

  /**
   * One old item into the combined one, under the lock: the combined item is read first (another window may have moved
   * it while this one waited), then the old item, then the combined item again before writing, so a value another
   * window saved meanwhile wins and a missing old item is reported only when the combined item lacks the id too.
   */
  const adopt = (id: string) => {
    const pending = migrating.get(id)
    if (pending) return pending
    const run = (async () => {
      const moved = await lock(async () => {
        const before = await fetch()
        if (before[id] !== undefined) return { token: before[id]!, readOld: false }
        const old = await items.get(id)
        const map = await fetch()
        if (map[id] !== undefined || old === null) return { token: map[id] ?? null, readOld: old !== null }
        map[id] = old
        await items.set(COMBINED_ACCOUNT, JSON.stringify(map))
        cached = { map, at: now() }
        return { token: old, readOld: true }
      })
      if (moved.readOld) await dropOld(id)
      return moved.token
    })().finally(() => migrating.delete(id))
    migrating.set(id, run)
    return run
  }

  return {
    async read(id) {
      const map = await current()
      if (map[id] !== undefined) return map[id]
      return adopt(id)
    },
    async write(id, token) {
      await update(map => { if (map[id] === token) return false; map[id] = token; return true })
      await dropOld(id)
    },
    async remove(id) {
      await update(map => { if (map[id] === undefined) return false; delete map[id]; return true })
      await items.delete(id)
    },
  }
}
