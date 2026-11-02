// The star prompt wired to the app: app-wide prefs, the onboarding mark, the local daemon and the toast (`starPrompt.ts`).
import { isCompleted } from '../onboarding/flow.ts'
import { reloadPrefs, writePref } from '../platform/prefs.ts'
import { openExternal } from '../settings/external.ts'
import { localClient } from '../state.ts'
import { toast } from './toasts.tsx'
import { COPY, forceStarPrompt, recordComposerSend, STAR_GLYPH, STAR_TOAST_ID, starKybern, type StarPromptDeps } from './starPrompt.ts'

function need() {
  const client = localClient()
  if (!client) throw new Error('Not connected')
  return client
}

const deps: StarPromptDeps = {
  read: reloadPrefs,
  write: writePref,
  onboarded: isCompleted,
  starred: async () => need().call('github.starred', {}),
  show: ({ canStar, onClose }) => {
    toast(COPY.title, {
      id: STAR_TOAST_ID,
      description: COPY.body,
      icon: { source: STAR_GLYPH, tone: 'warning' },
      duration: Infinity,
      onClose,
      cancel: { label: COPY.dismiss },
      action: {
        label: COPY.star,
        onClick: () => void starKybern(canStar, {
          star: async () => { await need().call('github.star', {}) },
          open: openExternal,
          thanked: () => toast.success(COPY.thanks, { description: COPY.thanksBody, icon: { source: STAR_GLYPH, tone: 'warning' } }),
          openFailed: () => toast.error(COPY.openFailed, { description: COPY.openFailedBody }),
        }),
      },
    })
  },
}

/** One message sent from a composer. Never throws, never waits the send. */
export function countComposerSend() {
  void recordComposerSend(deps).catch((error) => console.error('star prompt', error))
}

/** At launch: the `KYBERN_STAR_PROMPT=force` prompt, for screenshots and checks. */
export function startStarPrompt() {
  forceStarPrompt(deps)
}
