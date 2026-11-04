import { Icon, glyph } from '../icons.tsx'
import { SpinIcon } from '../kit/Spin.tsx'
import { N } from '../nav/glyphs.ts'
import { Link01Icon, LinkSquare02Icon, Robot01Icon } from '@hugeicons/core-free-icons'
import { useEffect, useMemo, useState } from 'react'
import { clearReviewDraft, reuseReviewDraftText, reviewDraftIsStale } from '@/state/prReviewModel'
import { agoPhrase } from '@/views/pullRequests/prFormat'
import { assessPullRequest, groupChecks, type CheckGroup } from '../../../../packages/kybern-client/src/pullRequests.ts'
import type { Project } from '../../../../packages/kybern-client/src/types.ts'
import { useApp } from '../state.ts'
import { Menu, MenuItem, MenuSeparator, Dialog, DialogHeader } from '../nav/overlay.tsx'
import { popoverStyle, PopupLayer } from '../composer/Menu.tsx'
import { getNav } from '../nav/store.ts'
import { ensureLoaded, useComposer } from '../composer/store.ts'
import { Label, REDUCED_MOTION, useTheme } from '../ui.tsx'
import { column, row } from '../usage/controls.tsx'
import { Button } from '../kit/Button.tsx'
import { IconButton } from '../kit/IconButton.tsx'
import { RadioRow } from '../kit/RadioRow.tsx'
import { Tabs, type TabsItem } from '../kit/Tabs.tsx'
import { TextArea } from '../kit/TextArea.tsx'
import { PrLoading, Problem, copy, openUrl } from './PrStates.tsx'
import { Rise, SwapOnChange } from '../motion/kit.tsx'
import { SuccessCheck } from '../motion/SuccessCheck.tsx'
import { SendPopover } from './PrSend.tsx'
import { Changes, Timeline } from './PrTabs.tsx'
import { useReview, type Review } from './useReview.ts'
import { Sidebar } from './detail/Sidebar.tsx'
import { Summary, SIDEBAR_BREAKPOINT } from './detail/Summary.tsx'
import { actionFailure, actionKind, agentPrompt, failingNames, type ActionRequest } from './detail/model.ts'
import { readView, saveView, type PrSection } from './viewState.ts'
import { useFlash } from './detail/useFlash.ts'
import { focusActions } from './actions/store.ts'
import { goPage } from '../nav/history.ts'

type Tab = PrSection
/** What a confirmation dialog is for: the merge and close of the verdict card, and "Check out in conversation" of the ⋯ menu. */
type Confirmation = { request: Extract<ActionRequest, { kind: 'merge' | 'close' }> | { kind: 'checkout' }; head: string }
const METHOD_WORDS = { squash: 'Squash merge', merge: 'Merge', rebase: 'Rebase and merge' } as const

export function Detail({ project, number, repo, onBack, width, onChanged, showBack = true, viewKey }: { viewKey?: string; project: Project; number: number; /** Group projects: the member repository the pull request is in. */ repo?: string; onBack: () => void; width: number; onChanged: () => void; showBack?: boolean }) {
  const t = useTheme()
  const review = useReview(project.id, number, repo)
  const memberName = repo ? project.members?.find(m => m.path === repo)?.name ?? repo : undefined
  const { detail, loading, error } = review
  const [tab, setTab] = useState<Tab>(() => viewKey ? readView(viewKey).section : 'summary')
  // A tab chosen by pointer rises in (`t-pane`); the keyboard shows it at once.
  const [paneIn, setPaneIn] = useState(false)
  const go = (next: Tab, byKey = false) => { setTab(next); if (viewKey) saveView(viewKey, { section: next }); setPaneIn(!byKey && !getNav().keyboardNav) }
  const [merged, flashMerged] = useFlash()
  const [sendOpen, setSendOpen] = useState(false)
  const [reviewOpen, setReviewOpen] = useState(false)
  const [confirmation, setConfirmation] = useState<Confirmation | null>(null)
  // The request a card button is running, and whether the last one failed (its words are `review.actionError`).
  const [pending, setPending] = useState<ActionRequest | null>(null)
  const [cardFailed, setCardFailed] = useState(false)
  const threads = useApp(s => s.threads)
  const settings = useComposer(s => s.settings)
  useEffect(() => { ensureLoaded() }, [])
  // Checking a member's pull request out into a conversation isn't supported yet.
  const linked = repo ? undefined : threads.find(th => th.project_id === project.id && th.worktree?.branch === detail?.pull_request.head && !th.subagent && !th.delegation)
  const pr = detail?.pull_request
  // The Timeline is one history of comments, reviews and commits: its badge is their total (the comment count when GitHub gave no totals).
  const counts = detail?.timeline_counts
  const timelineTotal = counts ? counts.comments + counts.reviews + counts.commits : detail?.comment_count
  const assessment = useMemo(() => detail ? assessPullRequest(detail) : null, [detail])
  const groups = useMemo(() => detail ? groupChecks(detail.checks) : [], [detail])
  // One bounded follow-up per head: GitHub computes mergeability lazily (and again after an update). No polling loop.
  const [retried, setRetried] = useState<string | null>(null)
  const retryKey = detail ? `${detail.head_sha}:${detail.pull_request.state}` : null
  useEffect(() => {
    if (!assessment?.refetch || !retryKey || retried === retryKey) return
    const timer = setTimeout(() => { setRetried(retryKey); review.refresh() }, 1800)
    return () => clearTimeout(timer)
  }, [assessment?.refetch, retryKey, retried])
  const openConfirmation = (request: Confirmation['request']) => { if (detail) { setConfirmation({ request, head: detail.head_sha }); setCardFailed(false) } }
  const promptContext = detail && pr ? { number, title: pr.title, url: pr.url, head: pr.head, base: pr.base, sha: detail.head_sha } : null
  const busyThread = linked?.status === 'running' || linked?.status === 'awaiting-approval'
  /** An agent button: the conversation linked to this branch if there is one, else a new one; it opens when the prompt is sent. */
  const startAgent = (topic: 'conflicts' | 'checks' | 'feedback', subject: string[] = []) => {
    if (!promptContext || pending) return
    setCardFailed(false); setPending({ kind: 'agent', topic: 'conflicts' })
    void review.repair(linked?.id, agentPrompt(topic, promptContext, subject)).then(ok => { setPending(null); if (!ok) setCardFailed(true) })
  }
  const request = (next: ActionRequest) => {
    if (!detail || pending) return
    if (next.kind === 'agent') { startAgent(next.topic); return }
    if (next.kind === 'merge' || next.kind === 'close') { openConfirmation(next); return }
    setCardFailed(false); setPending(next)
    void review.submit(actionKind(next), detail.head_sha, undefined, next.kind === 'enable_auto_merge' ? { merge_method: next.method } : {}).then(ok => { setPending(null); if (ok) onChanged(); else setCardFailed(true) })
  }
  const requestChanges = detail?.reviews?.filter(r => r.state === 'CHANGES_REQUESTED').map(r => r.login) ?? []
  const side = detail && assessment && <Sidebar detail={detail} assessment={assessment} groups={groups} compact={width < SIDEBAR_BREAKPOINT} pending={pending} error={cardFailed && !confirmation ? `${actionFailure(review.actionError ?? 'The action failed')} Check the pull request on GitHub and try again.` : null} canAgent={!repo && !busyThread && !!settings}
    onRequest={request} onFixChecks={() => startAgent('checks', failingNames(groups))} onFixGroup={(group: CheckGroup) => startAgent('checks', [group.label])} onAddressFeedback={() => startAgent('feedback', requestChanges)} onTimeline={() => go('timeline')}
    onOpenActions={pr ? () => { goPage('pulls'); focusActions({ projectId: project.id, ...(repo ? { repo } : {}), branch: pr.head }) } : undefined} />
  return <div testId="pr-detail" style={{ ...column, flexGrow: 1, minHeight: 0 }}>
    <div style={{ ...row, flexWrap: 'wrap', gap: 6, minHeight: 46, paddingLeft: 12, paddingRight: 12, paddingTop: 4, paddingBottom: 4, borderBottomWidth: 1, borderColor: t.border }}>
      {showBack && <IconButton icon={N.arrowLeft} label="Back to pull requests" testId="pr-back" onClick={onBack} />}
      {/* Summary, Changes and Timeline are tabs (ADE-55 §2.7), not a segmented control. */}
      <div style={{ ...row, height: 38, flexShrink: 1, minWidth: 0 }}>
        <Tabs testId="pr-tab" minWidth={0} overflow={false} animate={false} underlineGap={9} active={tab} onSelect={(key) => go(key as Tab)}
          items={(['summary', 'changes', 'timeline'] as const).map((key): TabsItem => ({
            key, label: key === 'summary' ? 'Summary' : key === 'changes' ? 'Changes' : 'Timeline', testId: `pr-tab-${key}`,
            ...(key === 'changes' && width >= 736 && pr?.additions != null && pr.deletions != null ? { extra: <div style={{ ...row, gap: 3 }}><Label color={t.green} size={12}>{`+${pr.additions}`}</Label><Label color={t.red} size={12}>{`−${pr.deletions}`}</Label></div> } : {}),
            ...(key === 'timeline' && timelineTotal != null ? { detail: String(timelineTotal) } : {}),
          }))} />
      </div>
      <div style={{ flexGrow: 1 }} />
      {loading && <SpinIcon testId="pr-detail-spinner" source={N.refresh} size={14} color={t.iconSecondary} />}
      {/* The three bare icon buttons are one menu; Merge lives in the verdict card. */}
      <div style={{ position: 'relative', display: 'flex' }}>
        <Menu testId="pr-more" align="end" trigger={() => <IconButton icon={N.ellipsis} label="More actions" testId="pr-more-trigger" />}>
          <MenuItem id="refresh" testId="pr-refresh-detail" label="Refresh" disabled={loading} onSelect={review.refresh} />
          <MenuItem id="copy" testId="pr-copy-link" label="Copy link" disabled={!pr} onSelect={() => pr && copy(pr.url)} />
          <MenuItem id="github" testId="pr-open-github" label="Open on GitHub" disabled={!pr} onSelect={() => pr && openUrl(pr.url)} />
          {detail && !repo && <MenuSeparator />}
          {detail && !repo && <MenuItem id="send" testId="pr-send" label="Send to agent…" onSelect={() => { setSendOpen(true); setReviewOpen(false) }} />}
          {detail && !repo && pr?.state === 'OPEN' && <MenuItem id="checkout" testId="pr-checkout" label="Check out in conversation" disabled={review.busy || !linked || busyThread} onSelect={() => openConfirmation({ kind: 'checkout' })} />}
        </Menu>
        {sendOpen && detail && <SendPopover projectId={project.id} review={review} close={() => setSendOpen(false)} onTimeline={() => go('timeline')} onChanges={() => go('changes')} />}
      </div>
      {detail && <>
        <div style={{ position: 'relative', display: 'flex' }}><Button label="Review" testId="pr-review" icon={N.pencil} chevron={width >= 672} onClick={() => { setReviewOpen(!reviewOpen); setSendOpen(false) }}>{width < 672 ? undefined : review.draft.inline.length ? `Review ${review.draft.inline.length}` : 'Review'}</Button>{reviewOpen && <ReviewPopover review={review} close={() => setReviewOpen(false)} onChanged={onChanged} />}</div>
        {merged && <div testId="pr-merged" style={{ ...row, gap: 6, height: 28, paddingLeft: 10, paddingRight: 10 }}><SuccessCheck size={16} color={t.green} /><Label color={t.green} size={14}>Merged</Label></div>}
      </>}
    </div>
    {error && <div style={{ padding: 24 }}><Problem error={error} projectId={project.id} noun="this pull request" refresh={review.refresh} /></div>}
    {!detail && !error && <div style={{ padding: 24 }}><PrLoading label="Loading pull request…" /></div>}
    {detail && <Rise key={tab} kind="pane" play={paneIn} style={{ flexGrow: 1, minHeight: 0 }}>{tab === 'summary' ? <Summary viewKey={viewKey} detail={detail} projectId={project.id} {...(repo ? { repo } : {})} repoName={memberName ?? project.name} width={width} side={side} /> : tab === 'changes' ? <Changes viewKey={viewKey} projectId={project.id} number={number} repo={repo} review={review} compact={width < 700} /> : <Timeline viewKey={viewKey} projectId={project.id} number={number} repo={repo} review={review} onSend={() => { setSendOpen(true); setReviewOpen(false) }} />}</Rise>}
    {confirmation && detail && <ConfirmDialog confirmation={confirmation} number={number} base={pr?.base} linkedTitle={linked?.title} review={review} close={() => setConfirmation(null)} onDone={(request) => { if (request.kind === 'merge') flashMerged(); setConfirmation(null); onChanged() }} linkedId={linked?.id} />}
  </div>
}

/** Merge, close and checkout ask first. "Merge anyway" (an admin merge past the branch rules) says so and is the destructive style. */
function ConfirmDialog({ confirmation, number, base, linkedTitle, linkedId, review, close, onDone }: { confirmation: Confirmation; number: number; base?: string; linkedTitle?: string; linkedId?: string; review: Review; close: () => void; onDone: (request: Confirmation['request']) => void }) {
  const t = useTheme()
  const r = confirmation.request
  const admin = r.kind === 'merge' && r.admin
  const anyway = admin || (r.kind === 'merge' && !!r.anyway)
  const title = r.kind === 'merge' ? anyway ? 'Merge anyway?' : 'Merge pull request?' : r.kind === 'close' ? 'Close pull request?' : 'Check out pull request?'
  const description = r.kind === 'merge'
    ? admin ? `#${number} doesn't meet the branch rules yet. Merging it as an administrator skips them and cannot be undone. ${METHOD_WORDS[r.method ?? 'squash']} at the reviewed head into ${base}.`
      : anyway ? `Some checks on #${number} failed or are still running. The branch rules don't require them, but a merge cannot be undone. ${METHOD_WORDS[r.method ?? 'squash']} at the reviewed head into ${base}.` : `${METHOD_WORDS[r.method ?? 'squash']} #${number} at the reviewed head into ${base}. The branch stays available.`
    : r.kind === 'close' ? `Close #${number} on GitHub. Its branch and your drafts remain available.`
    : `Check out #${number} in ${linkedTitle ?? 'the linked conversation'}. Running work, dirty files and open terminals block checkout.`
  const label = r.kind === 'merge' ? anyway ? 'Merge anyway' : 'Merge pull request' : r.kind === 'close' ? 'Close pull request' : 'Check out pull request'
  const submit = () => { void review.submit(r.kind, confirmation.head, r.kind === 'checkout' ? linkedId : undefined, r.kind === 'merge' ? { merge_method: r.method, ...(r.admin ? { admin: true } : {}) } : {}).then(ok => { if (ok) onDone(r) }) }
  return <Dialog instant={getNav().keyboardNav} testId="pr-confirm" onClose={() => { if (!review.busy) close() }}><DialogHeader title={title} description={description} onClose={() => { if (!review.busy) close() }} />{review.actionError && <div style={{ padding: 16 }}><Label color={t.red}>{`${actionFailure(review.actionError)} Your draft is kept.`}</Label></div>}<div style={{ ...row, justifyContent: 'flex-end', gap: 8, padding: 16 }}><Button size="lg" disabled={review.busy} onClick={close}>Cancel</Button><Button size="lg" variant={anyway || r.kind === 'close' ? 'destructive' : 'primary'} testId="pr-confirm-submit" disabled={review.busy} onClick={submit}>{review.busy ? 'Working…' : label}</Button></div></Dialog>
}

function ReviewPopover({ review, close, onChanged }: { review: Review; close: () => void; onChanged: () => void }) {
  const t = useTheme(); const [verdict, setVerdict] = useState<'comment' | 'approve' | 'request_changes'>('comment')
  const { draft, detail, busy } = review
  const stale = detail ? reviewDraftIsStale(draft, detail.head_sha) : false
  const empty = !draft.body.trim() && !draft.inline.length
  const isOpen = detail?.pull_request.state === 'OPEN'
  const effective = verdict !== 'comment' && (!isOpen || stale || (verdict === 'request_changes' && empty)) ? 'comment' : verdict
  const submitLabel = effective === 'approve' ? 'Approve' : effective === 'request_changes' ? 'Request changes' : `Post comment${draft.inline.length ? ` and ${draft.inline.length} inline ${draft.inline.length === 1 ? 'draft' : 'drafts'}` : ''}`
  return <PopupLayer still={getNav().keyboardNav || REDUCED_MOTION} testId="pr-review-popover" side="bottom" align="end" sideOffset={6} tabIndex={0} onMouseDownOutside={close} onKeyDown={e => { if (e.key === 'escape') close() }} style={{ ...popoverStyle(t), ...column, width: 416, padding: 16, gap: 8 }}>
    <div style={column}><Label color={t.text} size={14} lineHeight={21} weight={500}>Review</Label><Label color={t.textSecondary} size={13} lineHeight={19.5}>Drafts stay here until GitHub accepts them.</Label></div>
    {stale && detail && <div style={{ ...column, gap: 8 }}><Label color={t.warning} size={14}>{`This draft refers to commit ${draft.sourceHead?.slice(0, 8) ?? 'unknown'}. The current commit is ${detail.head_sha.slice(0, 8)}. Review its changes before reusing your text. Inline comments need new line anchors.`}</Label><Button size="sm" disabled={busy || !!draft.inline.length || !!draft.pendingInline} onClick={() => review.saveDraft(reuseReviewDraftText(draft, detail.head_sha))}>Use draft text for current commit</Button><Button size="sm" disabled={busy} onClick={() => review.saveDraft(clearReviewDraft(draft))}>Clear review draft</Button></div>}
    {draft.inline.length > 0 && <div style={{ ...column, gap: 4 }}><Label color={t.textSecondary} size={13}>{`${draft.inline.length} inline ${draft.inline.length === 1 ? 'comment' : 'comments'}`}</Label><virtual-list estimatedItemHeight={48} style={{ height: Math.min(144, draft.inline.length * 48) }}>{draft.inline.map((c, i) => <div key={`${i}:${c.path}`} style={{ ...row, paddingTop: 4, paddingBottom: 4 }}><Label color={t.textSecondary} size={13} truncate style={{ flexGrow: 1 }}>{`${c.path}:${c.line} · ${c.body}`}</Label><Button variant="ghost" size="sm" onClick={() => review.updateDraft({ inline: draft.inline.filter((_, j) => i !== j) })}>Remove draft</Button></div>)}</virtual-list></div>}
    <Label color={t.textSecondary} size={13}>Summary</Label><TextArea label="Review summary" testId="pr-review-body" value={draft.body} onChange={v => review.updateDraft({ body: v.slice(0, 65536) })} minRows={4} maxRows={6} />
    <div role="radiogroup" aria-label="Review type" style={{ ...column, gap: 2 }}>{([['comment', 'Comment', 'Post feedback without a verdict.'], ['approve', 'Approve', 'Approve these changes.'], ['request_changes', 'Request changes', 'Block merging until these are addressed.']] as const).map(([value, label, hint]) => {
      const disabled = value !== 'comment' && (!isOpen || stale || (value === 'request_changes' && empty))
      return <RadioRow key={value} testId={`pr-verdict-${value}`} checked={effective === value} title={label} description={hint} disabled={disabled} onSelect={() => setVerdict(value)} />
    })}</div>
    {review.actionError && <Label color={t.red} size={14}>{`${review.actionError} Your draft is kept.`}</Label>}
    <div style={{ ...row, justifyContent: 'flex-end', gap: 8 }}><Button variant="ghost" onClick={close}>Cancel</Button><Button testId="pr-review-submit" variant="primary" disabled={busy || stale || (effective !== 'approve' && empty)} onClick={() => { void review.submit(effective).then(ok => { if (ok) { close(); onChanged() } }) }}>{busy ? 'Submitting…' : submitLabel}</Button></div>
  </PopupLayer>
}
