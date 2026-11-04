// The Timeline tab: one chronological history of the pull request (comments, reviews with their inline comments, pushes,
// force pushes, review requests and state changes), filtered All / Comments / Reviews / Commits. Replaces the old
// Comments / Reviews / Inline segmented switch. Selecting a comment, a review or an inline comment still feeds Send to agent.
import {
  CheckmarkCircle02Icon, Comment01Icon, File01Icon, GitCommitIcon, GitMergeIcon, GitPullRequestClosedIcon, GitPullRequestDraftIcon,
  GitPullRequestIcon, Message01Icon, MessageEdit01Icon, Refresh01Icon, UserAdd01Icon,
} from '@hugeicons/core-free-icons'
import { memo, useEffect, useMemo, useState } from 'react'
import { relativeTime } from '@/lib/format'
import { toggleReviewFinding } from '@/state/prReviewModel'
import type { PrTimelineEvent } from '../../../../../packages/kybern-client/src/types.ts'
import { glyph, Icon } from '../../icons.tsx'
import { Button } from '../../kit/Button.tsx'
import { Checkbox } from '../../kit/Checkbox.tsx'
import { PickerChip } from '../../kit/PickerChip.tsx'
import { Segmented } from '../../kit/Segmented.tsx'
import { RADIUS } from '../../kit/controls.ts'
import { FONT_MONO } from '../../theme.ts'
import { alpha, cachedMarkdownTheme as markdownTheme } from '../../transcript/kit.tsx'
import { ink, Label, useTheme } from '../../ui.tsx'
import { column, row } from '../../usage/controls.tsx'
import { PR_SCROLL_LAYOUT } from '../scroll-layout.ts'
import { openUrl, PrLoading, Problem } from '../PrStates.tsx'
import type { Review } from '../useReview.ts'
import { Avatar } from '../../kit/Avatar.tsx'
import { findingFor, filterCounts, reviewFindings, reviewVerb, shortSha, timelineItems, TIMELINE_FILTERS, type Finding, type TimelineFilter, type TimelineItem } from './model.ts'
import { useReadingPosition } from '../useReadingPosition.ts'
import { readView, saveView } from '../viewState.ts'
import { useTimeline } from './useTimeline.ts'

const DOT = {
  opened: glyph(GitPullRequestIcon), comment: glyph(Message01Icon), commented: glyph(Comment01Icon), approved: glyph(CheckmarkCircle02Icon), changes: glyph(MessageEdit01Icon),
  commit: glyph(GitCommitIcon), force: glyph(Refresh01Icon), requested: glyph(UserAdd01Icon), draft: glyph(GitPullRequestDraftIcon), merged: glyph(GitMergeIcon),
  closed: glyph(GitPullRequestClosedIcon), file: glyph(File01Icon),
}
const MAX_WIDTH = 780
const COMMITS_SHOWN = 6

export function Timeline({ projectId, number, repo, review, onSend, viewKey }: { viewKey?: string; projectId: string; number: number; repo?: string; review: Review; onSend: () => void }) {
  const t = useTheme()
  const [filter, setFilter] = useState<TimelineFilter>(() => { const value = viewKey ? readView(viewKey).filter : 'all'; return TIMELINE_FILTERS.some(f => f.value === value) ? value as TimelineFilter : 'all' })
  const [inline, setInline] = useState(() => viewKey ? readView(viewKey).inline : true)
  const [restorePages] = useState(() => viewKey ? readView(viewKey).timelinePages : 1)
  const { events: loaded, hasMore, loading, error, more, full, page, restoring } = useTimeline(projectId, number, repo, review.revision, review.detail?.head_sha, restorePages)
  useEffect(() => { if (viewKey && page > readView(viewKey).timelinePages) saveView(viewKey, { timelinePages: page }) }, [viewKey, page])
  const scroll = useReadingPosition(viewKey, `timeline:${filter}`, (loaded.length > 0 || !loading) && !restoring)
  // Send to agent checks the pull request out into a chat, which member pull requests can't do yet.
  const selectable = !repo
  const pr = review.detail?.pull_request
  // The pull request's own opening is not a timeline item on GitHub; it leads the history.
  const events = useMemo<PrTimelineEvent[]>(() => pr?.created_at ? [{ id: `opened:${pr.number}`, kind: 'ready_for_review', created_at: pr.created_at, author: pr.author, ...(pr.author_avatar_url ? { avatar_url: pr.author_avatar_url } : {}), ...(pr.author_is_bot ? { author_is_bot: true } : {}) }, ...loaded] : loaded, [pr, loaded])
  const items = useMemo(() => timelineItems(events, filter), [events, filter])
  const counts = filterCounts(loaded, review.detail?.timeline_counts)
  const selected = review.draft.selected
  const toggle = (finding: Finding) => review.saveDraft(toggleReviewFinding(review.draft, finding.id, finding.entry))
  const select = (findings: Finding[]) => {
    let draft = review.draft
    for (const f of findings) if (!draft.selected.includes(f.id)) draft = toggleReviewFinding(draft, f.id, f.entry)
    review.saveDraft(draft)
    onSend()
  }
  return <div testId="pr-timeline" style={{ ...column, flexGrow: 1, minHeight: 0 }}>
    <div style={{ ...row, gap: 8, paddingLeft: 24, paddingRight: 24, paddingTop: 16, paddingBottom: 10, flexShrink: 0 }}>
      <Segmented size="sm" aria-label="Timeline filter" value={filter} options={TIMELINE_FILTERS.map(f => ({ value: f.value, label: f.label, ...(review.detail?.timeline_counts || loaded.length ? { count: counts[f.value] } : {}) }))} onChange={v => { setFilter(v as TimelineFilter); if (viewKey) saveView(viewKey, { filter: v }) }} tone="neutral" testId="pr-timeline-filter" />
      <div style={{ flexGrow: 1 }} />
      <PickerChip size="sm" quiet chevron={false} selected={inline} neutral testId="pr-timeline-inline" onClick={() => { setInline(!inline); if (viewKey) saveView(viewKey, { inline: !inline }) }}>Inline comments</PickerChip>
    </div>
    {error && !loaded.length ? <div style={{ padding: 24 }}><Problem error={error} projectId={projectId} noun="this pull request" refresh={review.refresh} /></div> : <virtual-list ref={scroll} {...PR_SCROLL_LAYOUT} testId="pr-timeline-entries" estimatedItemHeight={96} style={{ flexGrow: 1, minHeight: 0 }}>
      {!pr || (loading && !loaded.length) ? <div key="loading" style={{ padding: 24 }}><PrLoading label="Loading timeline…" rows={5} /></div> : items.length === 0 ? <div key="empty" style={{ padding: 24 }}><Label color={t.textSecondary} size={13}>{filter === 'all' ? 'Nothing has happened on this pull request yet.' : `No ${filter} ${hasMore ? 'in what has loaded so far.' : 'on this pull request.'}`}</Label></div> : items.map((item, i) => <TimelineRow key={item.id} item={item} last={i === items.length - 1 && !hasMore && !full} inline={inline} selectable={selectable} selected={selected} onToggle={toggle} onAddress={selectable ? select : undefined} />)}
      {pr && <div key="more" style={{ ...row, gap: 8, paddingLeft: 24, paddingRight: 24, paddingBottom: 24, paddingTop: 4 }}>
        {error && <Label color={t.warning} size={12}>{error.message.slice(0, 160)}</Label>}
        {hasMore && <Button testId="pr-timeline-more" size="sm" busy={loading} disabled={loading} onClick={more}>{loading ? 'Loading…' : 'Load more'}</Button>}
        {full && <Button size="sm" variant="ghost" onClick={() => openUrl(pr.url)}>Open on GitHub to read the rest</Button>}
      </div>}
    </virtual-list>}
    {selectable && selected.length > 0 && <div style={{ ...row, justifyContent: 'center', paddingBottom: 12, paddingTop: 4, flexShrink: 0 }}><Label color={t.text} size={13}>{`${selected.length} selected`}</Label><Button variant="ghost" size="sm" onClick={() => review.updateDraft({ selected: [], findings: {} })}>Clear</Button><Button size="sm" icon="bot" onClick={onSend}>Send to agent</Button></div>}
  </div>
}

function dotFor(event: PrTimelineEvent): { icon: string; tone: 'green' | 'red' | 'warning' | 'link' | 'plain' } {
  switch (event.kind) {
    case 'comment': return { icon: DOT.comment, tone: 'plain' }
    case 'review': {
      const state = (event.state ?? '').toUpperCase()
      return state === 'APPROVED' ? { icon: DOT.approved, tone: 'green' } : state === 'CHANGES_REQUESTED' ? { icon: DOT.changes, tone: 'red' } : { icon: DOT.commented, tone: 'plain' }
    }
    case 'commit': return { icon: DOT.commit, tone: 'plain' }
    case 'force_push': return { icon: DOT.force, tone: 'warning' }
    case 'review_requested': return { icon: DOT.requested, tone: 'plain' }
    case 'ready_for_review': return { icon: DOT.opened, tone: 'green' }
    case 'converted_to_draft': return { icon: DOT.draft, tone: 'plain' }
    case 'merged': return { icon: DOT.merged, tone: 'link' }
    case 'closed': return { icon: DOT.closed, tone: 'red' }
    case 'reopened': return { icon: DOT.opened, tone: 'green' }
  }
}

/** The line after the author's name. */
function describe(event: PrTimelineEvent): string {
  switch (event.kind) {
    case 'comment': return 'commented'
    case 'review': return reviewVerb(event.state)
    case 'commit': return 'pushed a commit'
    case 'force_push': return 'force-pushed'
    case 'review_requested': return `requested a review${event.target ? ` from ${event.target}` : ''}`
    case 'ready_for_review': return event.id.startsWith('opened:') ? 'opened this pull request' : 'marked this pull request as ready for review'
    case 'converted_to_draft': return 'converted this pull request to a draft'
    case 'merged': return 'merged this pull request'
    case 'closed': return 'closed this pull request'
    case 'reopened': return 'reopened this pull request'
  }
}

const TimelineRow = memo(function TimelineRow({ item, last, inline, selectable, selected, onToggle, onAddress }: { item: TimelineItem; last: boolean; inline: boolean; selectable: boolean; selected: string[]; onToggle: (finding: Finding) => void; onAddress?: (findings: Finding[]) => void }) {
  const t = useTheme()
  const push = item.type === 'push' ? item : null
  const event = item.type === 'event' ? item.event : null
  const dot = push ? { icon: DOT.commit, tone: 'plain' as const } : dotFor(event!)
  const color = dot.tone === 'green' ? t.green : dot.tone === 'red' ? t.red : dot.tone === 'warning' ? t.warning : dot.tone === 'link' ? t.link : t.iconSecondary
  const author = push ? push.author : event!.author
  const avatar = push ? push.avatar_url : event!.avatar_url
  const when = push ? push.created_at : event!.created_at
  const finding = event ? findingFor(event) : null
  const verb = push ? `pushed ${push.commits.length} ${push.commits.length === 1 ? 'commit' : 'commits'}` : describe(event!)
  return <div style={{ display: 'flex', justifyContent: 'center', paddingLeft: 24, paddingRight: 24 }}>
    <div testId={`pr-timeline-${item.id}`} style={{ ...row, alignItems: 'flex-start', gap: 12, width: '100%', maxWidth: MAX_WIDTH, position: 'relative', paddingBottom: 18 }}>
      {!last && <div style={{ position: 'absolute', left: 13, top: 30, bottom: 0, width: 1, backgroundColor: t.border }} />}
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'center', width: 28, height: 28, borderRadius: 14, flexShrink: 0, backgroundColor: t.v('--background'), borderWidth: 1, borderColor: dot.tone === 'red' ? alpha(t.red, .35) : t.border }}><Icon source={dot.icon} size={14} color={color} /></div>
      <div style={{ ...column, flexGrow: 1, minWidth: 0, flexShrink: 1 }}>
        <div style={{ ...row, gap: 6, minHeight: 28 }}>
          <Avatar login={author} url={avatar} size={16} />
          <Label color={t.text} size={13} lineHeight={18} weight={500}>{author || 'Someone'}</Label>
          <Label color={t.textSecondary} size={13} lineHeight={18} truncate style={{ flexGrow: 1 }}>{verb}</Label>
          {event?.kind === 'force_push' && event.before_sha && <Label color={t.textSecondary} size={12} lineHeight={16} font={FONT_MONO}>{`${shortSha(event.before_sha)} → ${shortSha(event.sha)}`}</Label>}
          <Label color={t.textTertiary} size={12} lineHeight={16}>{relativeTime(when)}</Label>
          {selectable && finding && <Checkbox testId={`pr-select-${item.id}`} aria-label="Select for agent" tone="neutral" checked={selected.includes(finding.id)} onCheckedChange={() => onToggle(finding)} />}
        </div>
        {push && <CommitList commits={push.commits} />}
        {event && finding && <Body text={event.body!} />}
        {event?.kind === 'review' && inline && event.inline_comments?.map((c, i) => <InlineComment key={i} path={c.path} line={c.line} body={c.body} finding={selectable ? findingFor(event, i) : null} selected={selected} onToggle={onToggle} />)}
        {event?.kind === 'review' && inline && (event.inline_count ?? 0) > (event.inline_comments?.length ?? 0) && <Label color={t.textTertiary} size={12} lineHeight={16} style={{ marginTop: 6 }}>{`and ${(event.inline_count ?? 0) - (event.inline_comments?.length ?? 0)} more inline ${(event.inline_count ?? 0) - (event.inline_comments?.length ?? 0) === 1 ? 'comment' : 'comments'} on GitHub`}</Label>}
        {event?.kind === 'review' && (event.state ?? '').toUpperCase() === 'CHANGES_REQUESTED' && onAddress && reviewFindings(event).length > 0 && <div style={{ ...row, gap: 6, marginTop: 8 }}><Button testId="pr-address-feedback" size="sm" icon="bot" onClick={() => onAddress(reviewFindings(event))}>Address feedback</Button></div>}
      </div>
    </div>
  </div>
})

/** The commits of one push: seven-character sha, then the first line. */
function CommitList({ commits }: { commits: PrTimelineEvent[] }) {
  const t = useTheme()
  const shown = commits.slice(0, COMMITS_SHOWN)
  return <div style={{ ...column, gap: 2, marginTop: 2 }}>
    {shown.map(c => <div key={c.id} style={{ ...row, gap: 8, minHeight: 20 }}>
      <Label color={t.textTertiary} size={12} lineHeight={20} font={FONT_MONO} style={{ flexShrink: 0 }}>{shortSha(c.sha)}</Label>
      <Label color={t.textSecondary} size={13} lineHeight={20} truncate style={{ flexGrow: 1 }}>{c.body ?? ''}</Label>
    </div>)}
    {commits.length > shown.length && <Label color={t.textTertiary} size={12} lineHeight={20}>{`and ${commits.length - shown.length} more`}</Label>}
  </div>
}

/** A comment or review summary in a raised bubble: Markdown, clamped at 16 lines with Show more. */
function Body({ text }: { text: string }) {
  const t = useTheme(); const [clamped, setClamped] = useState(true)
  const lines = text.split('\n'); const tall = lines.length > 16
  return <div style={{ ...column, gap: 4, marginTop: 6, paddingLeft: 12, paddingRight: 12, paddingTop: 8, paddingBottom: tall ? 6 : 8, borderRadius: RADIUS.lg, backgroundColor: ink(t, .035), borderWidth: 1, borderColor: t.cardEdge }}>
    <markdown source={tall && clamped ? lines.slice(0, 16).join('\n') : text} theme={markdownTheme(t)} onLinkClick={e => e.value && openUrl(e.value)} style={{ userSelect: 'text', color: t.text, fontSize: 14 }} />
    {tall && <Button variant="ghost" size="sm" style={{ alignSelf: 'flex-start' }} onClick={() => setClamped(!clamped)}>{clamped ? 'Show more' : 'Show less'}</Button>}
  </div>
}

/** One inline comment of a review: the file and line it is on, then what was said. */
function InlineComment({ path, line, body, finding, selected, onToggle }: { path: string; line?: number; body: string; finding: Finding | null; selected: string[]; onToggle: (finding: Finding) => void }) {
  const t = useTheme()
  return <div style={{ ...column, marginTop: 6, borderRadius: RADIUS.md, borderWidth: 1, borderColor: t.border, overflow: 'hidden' }}>
    <div style={{ ...row, gap: 6, paddingLeft: 10, paddingRight: 8, height: 28, backgroundColor: ink(t, .035), borderBottomWidth: 1, borderColor: t.border }}>
      <Icon source={DOT.file} size={12} color={t.iconSecondary} />
      <Label color={t.textSecondary} size={12} lineHeight={16} font={FONT_MONO} truncate style={{ flexGrow: 1 }}>{path}</Label>
      {line ? <Label color={t.textTertiary} size={12} lineHeight={16}>{`line ${line}`}</Label> : <Label color={t.textTertiary} size={12} lineHeight={16}>outdated</Label>}
      {finding && <Checkbox testId="pr-select-inline" aria-label="Select inline comment for agent" tone="neutral" checked={selected.includes(finding.id)} onCheckedChange={() => onToggle(finding)} />}
    </div>
    <div style={{ paddingLeft: 10, paddingRight: 10, paddingTop: 6, paddingBottom: 6 }}><Label color={t.text} size={13} lineHeight={20} style={{ userSelect: 'text' }}>{body}</Label></div>
  </div>
}
