// The words a pull request, Actions or review surface shows for a `Problem`, keyed by its kind and subject. The daemon's
// `title` and `hint` are only the fallback for kinds this client has no copy for.
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

/** `noun`: what the surface shows, e.g. "pull requests", "workflow runs", "this pull request". */
export function problemCopy(problem: Problem, noun: string): ProblemCopy {
  const gh = problem.subject === 'gh'
  if (gh && problem.kind === 'tool_missing') {
    return { title: `Install GitHub CLI to see ${noun}`, body: 'Kybern uses your own GitHub sign-in through the GitHub CLI (gh). Install it once and every project can use it.', action: 'install-gh', primaryLabel: 'Install GitHub CLI', retryLabel: 'Check again' }
  }
  if (gh && problem.kind === 'signed_out') {
    return { title: `Sign in to GitHub to see ${noun}`, body: 'The GitHub CLI (gh) is installed but isn’t signed in. Kybern uses your own GitHub sign-in through it.', action: 'sign-in-gh', primaryLabel: 'Sign in to GitHub', retryLabel: 'Check again' }
  }
  if (problem.kind === 'permission') {
    return { title: 'GitHub refused access', body: 'The signed-in GitHub account can’t see or change this repository. Check its access on GitHub, then try again.', action: null, retryLabel: 'Try again' }
  }
  if (problem.kind === 'not_found' && problem.subject === 'github_remote') {
    return { title: 'This project isn’t on GitHub', body: 'Add a GitHub remote to this repository, then try again.', action: null, retryLabel: 'Try again' }
  }
  if (problem.kind === 'not_found') {
    return { title: 'GitHub couldn’t find this', body: 'Check that origin points at GitHub and that the signed-in account can see the repository, then try again.', action: null, retryLabel: 'Try again' }
  }
  if (problem.kind === 'internal' || !problem.title) {
    return { title: `Unable to load ${noun}`, body: 'Something went wrong while talking to GitHub. Try again. If it keeps happening, copy the details and report them.', action: null, retryLabel: 'Try again' }
  }
  return { title: problem.title, body: problem.hint ?? 'Try again.', action: null, retryLabel: 'Try again' }
}
