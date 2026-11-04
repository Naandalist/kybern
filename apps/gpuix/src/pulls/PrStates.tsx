import { ghProblem } from '@/state/prReviewModel'
import { errorMessage } from '../../../../packages/kybern-client/src/problem.ts'
import { ProblemState } from '../kit/ProblemState.tsx'
import { TerminalSheetView } from '../onboarding/TerminalSheet.tsx'
import { closeGhSheet, GH_INSTALL_URL, ghSheet, ghSheetExited, installGh, signInGh } from './ghSetup.ts'
import { problemCopy } from './problemCopy.ts'
import { Skeleton } from '../kit/Skeleton.tsx'
import { DURATION, EASE } from '../motion/tokens.ts'
import { PATCHED_NATIVE } from '../native/local-build.ts'
import { Label, useTheme } from '../ui.tsx'
import { Button } from '../kit/Button.tsx'
import { column, row } from '../usage/controls.tsx'
import { openQuietly, writeClipboardText } from '../platform/os.ts'
export function openUrl(url: string) { if (/^https?:\/\//i.test(url)) openQuietly(url) }
export function copy(text: string) { void writeClipboardText(text).catch(() => {}) }
/**
 * A pull request, Actions or review surface that couldn't load. `error` is what the call rejected with (its
 * `data.problem` picks the recovery) or text. `partial`: other pull requests did load, so `error` (one line per
 * project or repository) is shown as it is. `projectId` names the machine an install or sign-in runs on.
 */
export function Problem({ error, refresh, partial = false, projectId, noun = 'pull requests' }: { error: unknown; refresh: () => void; partial?: boolean; projectId?: string; noun?: string }) {
  const t = useTheme()
  if (partial) return <div role="alert" style={{ ...column, gap: 8 }}><Label color={t.text} size={14} weight={500}>Some pull requests didn’t load</Label><Label color={t.textSecondary} size={13} lineHeight={20}>{errorMessage(error)}</Label><div style={row}><Button size="sm" onClick={refresh}>Try again</Button></div></div>
  const problem = ghProblem(error)
  const copy = problemCopy(problem, noun)
  const fix = copy.action === 'install-gh' ? () => projectId ? installGh(projectId, refresh) : openUrl(GH_INSTALL_URL)
    : copy.action === 'sign-in-gh' && projectId ? () => signInGh(projectId, refresh) : undefined
  return <ProblemState testId="pr-problem" title={copy.title} body={copy.body} detail={problem.detail}
    primary={fix && copy.primaryLabel ? { label: copy.primaryLabel, onClick: fix } : undefined}
    secondary={{ label: copy.retryLabel, onClick: refresh }} />
}

/** The GitHub CLI's install or sign-in terminal, once per window. */
export function GhSetupSheet() {
  const sheet = ghSheet.use(s => s.sheet)
  return <TerminalSheetView testId="gh-sheet" sheet={sheet} onClose={closeGhSheet} onExit={ghSheetExited} />
}

/**
 * Loading state of a pull request surface: skeleton rows that fade in after 150 ms (a fast answer never flashes them; the delay
 * is the fade's own `delay`, no timer). `list` draws pull request rows, `page` a heading, a meta line and paragraph lines.
 */
export function PrLoading({ label, kind = 'page', rows = 4 }: { label: string; kind?: 'list' | 'page'; rows?: number }) {
  const fade = PATCHED_NATIVE ? { initial: { opacity: 0 }, animate: { opacity: 1 }, transition: { delay: 0.15, duration: DURATION.quick, ease: [...EASE.out] as [number, number, number, number] } } : undefined
  const widths = ['100%', '92%', '97%', '64%', '88%'] as const
  return (
    <div role="status" aria-label={label} testId="pr-loading" paintTransform={fade} style={{ ...column, gap: kind === 'list' ? 14 : 12, width: '100%' }}>
      {kind === 'list'
        ? Array.from({ length: rows }, (_, i) => <div key={i} style={{ ...column, gap: 8 }}><Skeleton width={i % 2 ? '58%' : '78%'} height={14} /><Skeleton width="44%" height={11} /></div>)
        : <>
          <Skeleton width={96} height={24} radius={12} />
          <Skeleton width="62%" height={26} />
          <Skeleton width="38%" height={14} />
          <div style={{ ...column, gap: 9, paddingTop: 8 }}>{Array.from({ length: rows }, (_, i) => <Skeleton key={i} width={widths[i % widths.length]} height={13} />)}</div>
        </>}
    </div>
  )
}
