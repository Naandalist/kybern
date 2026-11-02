import { describe, expect, test } from 'bun:test'
import { DISMISSED_KEY, recordComposerSend, REPOSITORY_URL, resetStarSession, SENT_KEY, starKybern, type StarPromptDeps } from './starPrompt.ts'
import type { GithubStarredResult } from '../../../../packages/kybern-client/src/types.ts'

interface Setup { sent?: number; readable?: boolean; writable?: boolean; onboarded?: boolean; starred?: GithubStarredResult | 'fail'; duringCheck?: (store: Map<string, unknown>) => void }

// A fresh `resetStarSession()` stands in for an app relaunch; the store survives it, as `app.json` does.
function setup({ sent = 0, readable = true, writable = true, onboarded = true, starred = { starred: false, can_star: true }, duringCheck }: Setup = {}) {
  const store = new Map<string, unknown>([[SENT_KEY, sent]])
  const shown: { canStar: boolean | null; onClose: () => void }[] = []
  let checks = 0
  const deps: StarPromptDeps = {
    read: () => { if (!readable) throw new Error('EACCES'); return Object.fromEntries(store) },
    write: (key, value) => { if (!writable) throw new Error('EROFS'); store.set(key, value) },
    onboarded: () => onboarded,
    starred: async () => {
      checks++
      duringCheck?.(store)
      if (starred === 'fail') throw new Error('Unknown method github.starred')
      return starred
    },
    show: (prompt) => { shown.push(prompt) },
  }
  const session = () => { resetStarSession(); return () => recordComposerSend(deps) }
  return { store, shown, deps, session, checks: () => checks }
}

describe('recordComposerSend', () => {
  test('asks on the sixth send, once per session, and never again once closed', async () => {
    const s = setup()
    let send = s.session()
    for (let i = 0; i < 5; i++) await send()
    expect(s.shown).toHaveLength(0)
    expect(s.store.get(SENT_KEY)).toBe(5)
    await send()
    await send()
    expect(s.shown).toHaveLength(1)
    expect(s.shown[0]!.canStar).toBe(true)

    // Ignored: a later session asks again, once.
    send = s.session()
    await send()
    await send()
    expect(s.shown).toHaveLength(2)

    // Closed by any path: never again.
    s.shown[1]!.onClose()
    expect(s.store.get(DISMISSED_KEY)).toBe(true)
    send = s.session()
    for (let i = 0; i < 10; i++) await send()
    expect(s.shown).toHaveLength(2)
  })

  test('counts nothing before onboarding is done', async () => {
    const s = setup({ sent: 5, onboarded: false })
    await s.session()()
    expect(s.store.get(SENT_KEY)).toBe(5)
    expect(s.shown).toHaveLength(0)
  })

  test('never asks when the prefs cannot be read or written', async () => {
    const unwritable = setup({ sent: 5, writable: false })
    await unwritable.session()()
    expect(unwritable.shown).toHaveLength(0)
    expect(unwritable.checks()).toBe(0)
    const unreadable = setup({ sent: 5, readable: false })
    await unreadable.session()()
    expect(unreadable.shown).toHaveLength(0)
  })

  test('an account that starred already saves the dismissal silently', async () => {
    const s = setup({ sent: 5, starred: { starred: true, can_star: true } })
    await s.session()()
    expect(s.shown).toHaveLength(0)
    expect(s.store.get(DISMISSED_KEY)).toBe(true)
    await s.session()()
    expect(s.checks()).toBe(1)
  })

  test('a failed check skips this session only', async () => {
    const s = setup({ sent: 5, starred: 'fail' })
    const send = s.session()
    await send()
    await send()
    expect(s.shown).toHaveLength(0)
    expect(s.checks()).toBe(1)
    expect(s.store.has(DISMISSED_KEY)).toBe(false)
  })

  test('a missing or signed-out gh still asks, and Star then opens the page', async () => {
    const s = setup({ sent: 5, starred: { starred: false, can_star: false } })
    await s.session()()
    expect(s.shown).toHaveLength(1)
    expect(s.shown[0]!.canStar).toBe(false)
  })

  test('never asks when another window closed its prompt during the check', async () => {
    const s = setup({ sent: 5, duringCheck: (store) => store.set(DISMISSED_KEY, true) })
    await s.session()()
    expect(s.shown).toHaveLength(0)
  })
})

describe('starKybern', () => {
  const run = (canStar: boolean | null, { star = true, open = true } = {}) => {
    const log: string[] = []
    const result = starKybern(canStar, {
      star: async () => { log.push('star'); if (!star) throw new Error('gh: Forbidden (HTTP 403)') },
      open: async (url) => { log.push(`open ${url}`); if (!open) throw new Error('no browser') },
      thanked: () => log.push('thanked'),
      openFailed: () => log.push('open failed'),
    })
    return result.then((outcome) => ({ outcome, log }))
  }

  test('stars through gh and thanks', async () => {
    expect(await run(true)).toEqual({ outcome: 'starred', log: ['star', 'thanked'] })
    expect(await run(null)).toEqual({ outcome: 'starred', log: ['star', 'thanked'] })
  })

  test('opens the repository without gh, or when starring fails', async () => {
    expect(await run(false)).toEqual({ outcome: 'opened', log: [`open ${REPOSITORY_URL}`] })
    expect(await run(true, { star: false })).toEqual({ outcome: 'opened', log: ['star', `open ${REPOSITORY_URL}`] })
    expect(await run(false, { open: false })).toEqual({ outcome: 'failed', log: [`open ${REPOSITORY_URL}`, 'open failed'] })
  })
})
