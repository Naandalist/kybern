// "Enjoying Kybern?" (ADE-117, after T3 Code's GitHubStarPrompt): once a person has sent more than five messages from a composer,
// a toast asks for a star on GitHub, at most once per app session, until it is closed once. Any close (its X, Dismiss or Star)
// saves the dismissal for good; a toast that is ignored can come back in a later session. The count and the dismissal are
// app-wide prefs (`platform/prefs.ts`), so every environment shares them; they are separate keys, so a count written by
// another window can never undo a dismissal. Only `ComposerView`'s sends count (new turns, steers, queued sends); imports,
// delegated agents and Coordinator turns never pass through a composer. Counting starts once onboarding is done.
//
// Before asking, the local daemon's `gh` says whether its account starred the repository (`github.starred`): starred saves the
// dismissal silently; a failed call (an older daemon, GitHub down) skips this session only; a missing or signed-out `gh`
// answers "not starred", so the toast shows and Star opens the repository page instead of `github.star`.
//
// `KYBERN_STAR_PROMPT=force` shows it at launch, skipping the count, the dismissal and the check (screenshots and checks).
import { StarIcon } from '@hugeicons/core-free-icons'
import type { GithubStarredResult } from '../../../../packages/kybern-client/src/types.ts'
import { glyph } from '../icons.tsx'

export const SENT_KEY = 'kybern.starPrompt.sent'
export const DISMISSED_KEY = 'kybern.starPrompt.dismissed'
export const REPOSITORY_URL = 'https://github.com/Tresnanda/kybern'
/** The toast asks on the send after this many (`sent > 5`). */
export const SENDS_BEFORE_ASKING = 5
export const FORCED = process.env.KYBERN_STAR_PROMPT === 'force'
export const STAR_TOAST_ID = 'github-star'

export const COPY = {
  title: 'Enjoying Kybern?',
  body: 'Give us a star on GitHub to let us know!',
  dismiss: 'Dismiss',
  star: 'Star on GitHub',
  thanks: 'Thanks for the star',
  thanksBody: 'It helps more people find Kybern.',
  openFailed: 'Unable to open GitHub',
  openFailedBody: 'Go to github.com/Tresnanda/kybern to star Kybern.',
} as const

/** What the prompt needs from the app; tests pass their own. */
export interface StarPromptDeps {
  /** Every app pref, read fresh from disk; throws when they cannot be read. */
  read: () => Record<string, unknown>
  /** Throws when the pref cannot be written. */
  write: (key: string, value: unknown) => void
  onboarded: () => boolean
  /** `github.starred` on the local daemon; rejects when the call fails. */
  starred: () => Promise<GithubStarredResult>
  /** Show the toast. `onClose` runs on every close path and saves the dismissal. */
  show: (prompt: { canStar: boolean | null; onClose: () => void }) => void
}

let shownThisSession = false
/** Settled for good (dismissed) or for this session (prefs unusable): later sends skip the disk read. */
let settled = false
/** Tests: a fresh app session. */
export function resetStarSession() { shownThisSession = false; settled = false }

function saveDismissal(deps: Pick<StarPromptDeps, 'read' | 'write'>) {
  try {
    deps.read()
    deps.write(DISMISSED_KEY, true)
    settled = true
  } catch { /* Unwritable prefs: the next session cannot have seen a count either. */ }
}

/** Count one message sent from a composer, and ask for a star when it is time (see the header). */
export async function recordComposerSend(deps: StarPromptDeps): Promise<void> {
  if (shownThisSession || settled || FORCED) return
  if (!deps.onboarded()) return
  let sent: number
  try {
    const prefs = deps.read()
    if (prefs[DISMISSED_KEY] === true) { settled = true; return }
    const before = typeof prefs[SENT_KEY] === 'number' && Number.isFinite(prefs[SENT_KEY]) ? (prefs[SENT_KEY] as number) : 0
    sent = before + 1
    deps.write(SENT_KEY, sent)
  } catch {
    // Prefs that cannot be read or written could not keep a dismissal either: never ask.
    settled = true
    return
  }
  if (sent <= SENDS_BEFORE_ASKING) return
  shownThisSession = true
  let state: GithubStarredResult
  try {
    state = await deps.starred()
  } catch {
    return // This session only: the dismissal is not saved.
  }
  if (state.starred) return saveDismissal(deps)
  try {
    // Another window can have closed its prompt while the check ran.
    if (deps.read()[DISMISSED_KEY] === true) return
  } catch {
    return
  }
  deps.show({ canStar: state.can_star, onClose: () => saveDismissal(deps) })
}

/** `KYBERN_STAR_PROMPT=force`: show it now, whatever the count, the dismissal and the account say. */
export function forceStarPrompt(deps: Pick<StarPromptDeps, 'show' | 'read' | 'write'>) {
  if (!FORCED || shownThisSession) return
  shownThisSession = true
  deps.show({ canStar: null, onClose: () => saveDismissal(deps) })
}

export interface StarDeps {
  /** `github.star` on the local daemon; rejects when `gh` is missing, signed out or GitHub refuses. */
  star: () => Promise<void>
  open: (url: string) => Promise<void>
  thanked: () => void
  openFailed: () => void
}

/**
 * The Star button: star through `gh` when it can (or when nobody knows, `canStar` null), and thank; otherwise, or when that
 * fails, open the repository page in the browser.
 */
export async function starKybern(canStar: boolean | null, deps: StarDeps): Promise<'starred' | 'opened' | 'failed'> {
  if (canStar !== false) {
    try {
      await deps.star()
      deps.thanked()
      return 'starred'
    } catch { /* Fall back to the page. */ }
  }
  try {
    await deps.open(REPOSITORY_URL)
    return 'opened'
  } catch {
    deps.openFailed()
    return 'failed'
  }
}

export const STAR_GLYPH = glyph(StarIcon)
