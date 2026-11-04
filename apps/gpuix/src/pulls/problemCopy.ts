// The words a pull request, Actions or review surface shows for a `Problem`. The copy itself lives in the shared
// `describeProblem`; this maps its view onto the PR page's gh fixes (install or sign in through the terminal sheet).
import { describeProblem } from '../../../../packages/kybern-client/src/problems.ts'
import type { Problem } from '../../../../packages/kybern-client/src/types.ts'

export interface ProblemCopy {
  title: string
  body: string
  /** The action that fixes it: `install_tool` / `sign_in` with their gh params, or none. */
  action: 'install-gh' | 'sign-in-gh' | null
  primaryLabel?: string
  /** The re-check button's label. */
  retryLabel: string
}

/**
 * `noun`: what the surface shows, e.g. "pull requests", "workflow runs", "this pull request". The technical text stays
 * behind Details (the problem's `detail`), so only the problem itself is described.
 */
export function problemCopy(problem: Problem, noun: string): ProblemCopy {
  const view = describeProblem({ message: '', data: { problem } }, { action: `load ${noun}`, goal: `see ${noun}` })
  const gh = view.subject === 'gh'
  const action = gh && view.action?.id === 'install_tool' ? 'install-gh' : gh && view.action?.id === 'sign_in' ? 'sign-in-gh' : null
  const retry = [view.action, view.secondary].find(a => a?.id === 'retry')
  return { title: view.title, body: view.body, action, ...(action ? { primaryLabel: view.action!.label } : {}), retryLabel: retry?.label ?? 'Try again' }
}
