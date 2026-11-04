// One workflow run (ADE-97, "C · Pipeline"): a header with its status, one line on what happened (where a failure runs, how far
// a running run is against its usual time, what a finished one did) and the run actions (rerun failed jobs, rerun all, cancel);
// the title with branch, commit, workflow and number, trigger, actor, age and duration; an attempt switcher; the jobs on their
// `needs` graph (`Graph.tsx`); and under it the selected job's steps beside its log. Narrow, the steps sit above the log.
// The run is read again every 10 s while it is in progress and the window is visible, every 60 s otherwise; a read that
// changes nothing keeps the previous result, so nothing re-renders.
import { asError } from '../../../../../packages/kybern-client/src/problem.ts'
import { useEffect, useMemo, useState } from 'react'
import { relativeTime } from '@/lib/format'
import { BUCKET_TONE, eventWord, isActive, refreshDelay, runBucket, runWord, type ActionsRepo } from '../../../../../packages/kybern-client/src/actions.ts'
import { buildGraph, columnWidths, duration, jobTimes, placeGraph, runSummary, seconds } from '../../../../../packages/kybern-client/src/actionsPipeline.ts'
import type { ActionsActionKind, ActionsRunResult, RunAttempt, WorkflowJob, WorkflowRun } from '../../../../../packages/kybern-client/src/types.ts'
import { Icon } from '../../icons.tsx'
import { Button } from '../../kit/Button.tsx'
import { RADIUS } from '../../kit/controls.ts'
import { IconButton } from '../../kit/IconButton.tsx'
import { StatusChip } from '../../kit/StatusChip.tsx'
import { toneColor } from '../../kit/tone.ts'
import { SwapOnChange } from '../../motion/kit.tsx'
import { N } from '../../nav/glyphs.ts'
import { Menu, MenuItem } from '../../nav/overlay.tsx'
import { useApp } from '../../state.ts'
import { FONT_MONO } from '../../theme.ts'
import { alpha } from '../../transcript/kit.tsx'
import { ink, Label, useTheme } from '../../ui.tsx'
import { column, row } from '../../usage/controls.tsx'
import { prCall } from '../client.ts'
import { copy, openUrl, PrLoading, Problem } from '../PrStates.tsx'
import { BUCKET_GLYPH, RunGlyph } from './glyph.tsx'
import { measureName, NODE_HEIGHT, PipelineGraph, ROW_GAP } from './Graph.tsx'
import { JobLog } from './JobLog.tsx'
import { LiveClock } from './LiveClock.tsx'
import { markStarted } from './watch.ts'

/** Below this width the steps stack above the log. */
const SPLIT = 760
const PAD = 24
const STOP = `<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none"><rect x="4.25" y="4.25" width="7.5" height="7.5" rx="1.5" stroke="#000" stroke-width="1.5"/></svg>`

/** The job a run opens on: the first failure, else the first one running, else the first. */
function firstJob(jobs: readonly WorkflowJob[]): WorkflowJob | undefined {
  return jobs.find(j => runBucket(j) === 'failed') ?? jobs.find(j => isActive(j)) ?? jobs[0]
}

/** Jobs' usual times per finished passing run, read once per run (a finished run never changes). */
const usualJobs = new Map<string, Map<string, number>>()

export function RunDetail({ target, runId, listed, reference, usual, width, height, showBack, visible, onBack, onActive, onChanged }: {
  target: ActionsRepo; runId: number; listed?: WorkflowRun
  /** A recent passing run of the same workflow: its job times are what a running job's progress measures against. */
  reference?: WorkflowRun
  /** The workflow's usual run time, in seconds. */
  usual: number | null
  width: number; height: number; showBack: boolean; visible: boolean
  onBack: () => void; onActive: (active: boolean) => void; onChanged: () => void
}) {
  const t = useTheme()
  const connected = useApp(s => s.status === 'open')
  const member = target.repo ? { repo: target.repo } : {}
  const [detail, setDetail] = useState<ActionsRunResult | null>(null)
  const [error, setError] = useState<Error | null>(null)
  const [revision, refresh] = useState(0)
  const [jobId, setJobId] = useState<number | null>(null)
  const [attempt, setAttempt] = useState<number | null>(null)
  const [busy, setBusy] = useState<ActionsActionKind | null>(null)
  const [actionError, setActionError] = useState<string | null>(null)
  const [notice, setNotice] = useState<string | null>(null)
  useEffect(() => {
    if (!connected) return
    let canceled = false
    void prCall(target.projectId, 'github.actions.run', { project_id: target.projectId, ...member, run_id: runId, ...(attempt ? { attempt } : {}) }).then(next => {
      if (canceled) return
      // An unchanged read keeps the previous object: no node, edge or log re-renders.
      setDetail(prev => prev && JSON.stringify(prev) === JSON.stringify(next) ? prev : next); setError(null)
    }, e => { if (!canceled) setError(asError(e)) })
    return () => { canceled = true }
  }, [runId, revision, connected, attempt])
  const run = detail?.run ?? listed
  const active = !!run && isActive(run)
  useEffect(() => { onActive(active); return () => onActive(false) }, [active])
  useEffect(() => {
    const delay = refreshDelay({ active, onScreen: true, windowVisible: visible })
    if (delay === null || !detail) return
    const timer = setTimeout(() => refresh(n => n + 1), delay)
    return () => clearTimeout(timer)
  }, [active, visible, detail])

  // A running run measures its jobs against a recent passing run of the workflow, read once.
  const [usualTimes, setUsualTimes] = useState<Map<string, number> | undefined>(() => reference ? usualJobs.get(`${runId}:${reference.id}`) : undefined)
  useEffect(() => {
    if (!active || !reference || !connected) return
    const key = `${runId}:${reference.id}`
    if (usualJobs.has(key)) { setUsualTimes(usualJobs.get(key)); return }
    let canceled = false
    void prCall(target.projectId, 'github.actions.run', { project_id: target.projectId, ...member, run_id: reference.id }).then(r => {
      const times = jobTimes(r.jobs)
      if (usualJobs.size > 64) usualJobs.clear()
      usualJobs.set(key, times)
      if (!canceled) setUsualTimes(times)
    }, () => {})
    return () => { canceled = true }
  }, [active, reference?.id, connected])

  const jobs = detail?.jobs ?? []
  const job = jobs.find(j => j.id === jobId) ?? firstJob(jobs)
  const graph = useMemo(() => buildGraph(jobs, detail?.workflow_jobs, { running: !!detail && isActive(detail.run) }), [detail])
  const inner = Math.max(240, width - PAD * 2)
  const layout = useMemo(() => {
    const cols = Math.max(1, graph.columns)
    // Natural widths, the widest giving way first when the pane is narrow; the gaps share what is left.
    const widths = columnWidths(graph, measureName, { min: 128, max: graph.fallback ? 380 : 300, budget: inner - 48 * (cols - 1) })
    const used = widths.reduce((a, b) => a + b, 0)
    const gap = cols > 1 ? Math.max(48, Math.min(140, Math.floor((inner - used) / (cols - 1)))) : 0
    return placeGraph(graph, { columnWidths: widths, nodeHeight: NODE_HEIGHT, rowGap: ROW_GAP, columnGap: gap })
  }, [graph, inner])

  const act = (action: ActionsActionKind, job?: WorkflowJob) => {
    if (!run || busy) return
    setBusy(action); setActionError(null); setNotice(null)
    void prCall(target.projectId, 'github.actions.action', { project_id: target.projectId, ...member, action, run_id: run.id, ...(job ? { job_id: job.id } : {}) }).then(() => {
      if (action !== 'cancel') { markStarted(run.id); setAttempt(null) }
      setNotice(action === 'cancel' ? 'Cancel requested. GitHub stops the jobs in a few seconds.' : 'Rerun requested. New jobs appear here in a few seconds.')
      setBusy(null); refresh(n => n + 1); onChanged()
    }, e => { setActionError(String(e)); setBusy(null) })
  }

  const split = width >= SPLIT
  const failed = !!run && runBucket(run) === 'failed'
  const summary = run && detail ? runSummary(run, graph, usual) : ''
  // The graph takes what it needs up to about a third of the window; a taller one scrolls.
  const graphMax = Math.max(168, Math.round(height * 0.36))
  return <div testId="actions-run" style={{ ...column, flexGrow: 1, minHeight: 0, minWidth: 0 }}>
    <div style={{ ...row, gap: 8, minHeight: 46, flexShrink: 0, paddingLeft: showBack ? 12 : PAD, paddingRight: 12, borderBottomWidth: 1, borderColor: t.border }}>
      {showBack && <IconButton icon={N.arrowLeft} label="Back to runs" testId="actions-back" onClick={onBack} />}
      {run && <RunStatus run={run} />}
      {summary && <div testId="actions-run-summary" style={{ display: 'flex', flexShrink: 1, minWidth: 0, paddingLeft: 4 }}><Label color={t.textSecondary} size={13} lineHeight={18} truncate>{summary}</Label></div>}
      <div style={{ flexGrow: 1 }} />
      {run && active && <Button size="sm" variant="secondary" danger icon={STOP} testId="actions-cancel" busy={busy === 'cancel'} disabled={!!busy} onClick={() => act('cancel')}>Cancel run</Button>}
      {run && !active && <Button size="sm" testId="actions-rerun-all" variant="secondary" busy={busy === 'rerun'} disabled={!!busy} onClick={() => act('rerun')}>Rerun all jobs</Button>}
      {run && !active && failed && <Button size="sm" variant="primary" testId="actions-rerun-failed" busy={busy === 'rerun_failed'} disabled={!!busy} onClick={() => act('rerun_failed')}>Rerun failed jobs</Button>}
      {run && <Menu testId="actions-run-more" align="end" trigger={() => <IconButton icon={N.ellipsis} label="More run actions" testId="actions-run-more-trigger" />}>
        <MenuItem id="refresh" label="Refresh" onSelect={() => refresh(n => n + 1)} />
        <MenuItem id="copy" label="Copy link" onSelect={() => copy(run.url)} />
        <MenuItem id="github" label="Open on GitHub" onSelect={() => openUrl(run.url)} />
      </Menu>}
    </div>
    {error && !detail && <div style={{ padding: 24 }}><Problem error={error} projectId={target.projectId} noun="this run" refresh={() => refresh(n => n + 1)} /></div>}
    {!run && !error && <div style={{ padding: 24 }}><PrLoading label="Loading run…" /></div>}
    {run && <>
      <div style={{ ...row, alignItems: 'flex-start', gap: 16, paddingLeft: PAD, paddingRight: PAD, paddingTop: 18, paddingBottom: 14, flexShrink: 0 }}>
        <RunSummary run={run} />
        <Attempts run={run} attempts={detail?.attempts ?? []} current={attempt} onPick={a => { setAttempt(a); setJobId(null) }} />
      </div>
      {(actionError || notice) && <div role={actionError ? 'alert' : 'status'} style={{ paddingLeft: PAD, paddingRight: PAD, paddingBottom: 10, flexShrink: 0 }}>
        <Label color={actionError ? t.red : t.textSecondary} size={12.5} lineHeight={18}>{actionError ?? notice!}</Label>
      </div>}
      <div testId="actions-graph-area" style={{ display: 'flex', flexShrink: 0, maxHeight: graphMax, overflowY: 'scroll', overflowX: 'scroll', paddingLeft: PAD, paddingRight: PAD, paddingTop: 4, paddingBottom: 20 }}>
        {!detail ? <div style={{ flexGrow: 1 }}><PrLoading label="Loading jobs…" kind="list" rows={3} /></div>
          : jobs.length === 0 ? <Label color={t.textSecondary} size={13} lineHeight={20}>{active ? 'Waiting for a runner to pick up the jobs.' : 'This run has no jobs.'}</Label>
          : <PipelineGraph graph={graph} layout={layout} selected={job?.id} onSelect={setJobId} usual={usualTimes} live={active} />}
      </div>
      <div style={{ display: 'flex', flexDirection: split ? 'row' : 'column', flexGrow: 1, minHeight: 0, borderTopWidth: 1, borderColor: t.surfaceDivider }}>
        {job && <Steps job={job} split={split} />}
        {job && <JobLog key={job.id} target={target} run={run} job={job} onRerun={() => act('rerun_job', job)} rerunBusy={busy === 'rerun_job'} />}
      </div>
    </>}
  </div>
}

function RunStatus({ run }: { run: WorkflowRun }) {
  const bucket = runBucket(run)
  return <SwapOnChange kind="state" active={bucket} style={{ flexShrink: 0 }}><StatusChip testId="actions-run-status" tone={BUCKET_TONE[bucket]} icon={BUCKET_GLYPH[bucket]}>{runWord(run)}</StatusChip></SwapOnChange>
}

/** The run's title and one line of facts: branch, commit, workflow and number, trigger and actor, age, duration. */
function RunSummary({ run }: { run: WorkflowRun }) {
  const t = useTheme()
  const active = isActive(run)
  const took = seconds(run.started_at ?? run.created_at, run.updated_at)
  const facts = [
    `${run.workflow_name} #${run.number}`,
    [eventWord(run.event), run.actor ? `by ${run.actor}` : ''].filter(Boolean).join(' '),
    ...(active ? [] : [`${relativeTime(run.created_at)} ago`.replace('now ago', 'just now'), took !== null ? duration(took) : '']),
  ].filter(Boolean)
  return <div style={{ ...column, gap: 6, flexGrow: 1, flexShrink: 1, minWidth: 0 }}>
    <div testId="actions-run-title" style={{ display: 'flex' }}><Label color={t.text} size={18} lineHeight={24} weight={600} style={{ lineClamp: 2, textOverflow: 'ellipsis' }}>{run.title || run.workflow_name}</Label></div>
    <div style={{ ...row, flexWrap: 'wrap', columnGap: 12, rowGap: 4 }}>
      {run.head_branch && <div style={{ ...row, gap: 4 }}><Icon source={N.branch} size={12} color={t.textSecondary} /><Label color={t.textSecondary} size={13} lineHeight={18}>{run.head_branch}</Label></div>}
      <Label color={t.textSecondary} size={12.5} lineHeight={18} font={FONT_MONO}>{run.head_sha.slice(0, 7)}</Label>
      <div style={{ ...row, gap: 0 }}>
        <Label color={t.textSecondary} size={13} lineHeight={18}>{facts.join(' · ')}</Label>
        {active && <LiveClock since={run.started_at ?? run.created_at} format="clock" color={t.textSecondary} size={13} lineHeight={18} prefix=" · running " />}
      </div>
    </div>
  </div>
}

/** One chip per attempt (the latest last), the one shown selected; a single attempt is a plain label. */
function Attempts({ run, attempts, current, onPick }: { run: WorkflowRun; attempts: RunAttempt[]; current: number | null; onPick: (attempt: number | null) => void }) {
  const t = useTheme()
  const list = attempts.length ? attempts : [{ attempt: run.attempt, status: run.status, conclusion: run.conclusion }]
  const shown = current ?? list.at(-1)!.attempt
  const latest = list.at(-1)!.attempt
  return <div testId="actions-attempts" role={list.length > 1 ? 'radiogroup' : undefined} aria-label="Attempt" style={{ ...row, gap: 2, flexShrink: 0, padding: 2, borderRadius: RADIUS.md + 2, borderWidth: 1, borderColor: t.cardEdge }}>
    {list.map(a => {
      const on = a.attempt === shown
      return <div key={a.attempt} testId={`actions-attempt-${a.attempt}`} role={list.length > 1 ? 'radio' : undefined} aria-checked={list.length > 1 ? on : undefined} tabIndex={list.length > 1 ? 0 : -1}
        onClick={() => { if (list.length > 1) onPick(a.attempt === latest ? null : a.attempt) }}
        onKeyDown={e => { if (list.length > 1 && (e.key === 'enter' || e.key === 'space')) onPick(a.attempt === latest ? null : a.attempt) }}
        style={{ ...row, gap: 6, height: 26, paddingLeft: 8, paddingRight: 10, borderRadius: RADIUS.md, backgroundColor: on ? ink(t, t.variant === 'dark' ? .08 : .06) : 'transparent', ...(list.length > 1 ? { cursor: 'pointer', hover: { backgroundColor: on ? ink(t, t.variant === 'dark' ? .08 : .06) : t.hover } } : {}) }}>
        <RunGlyph item={a} size={13} />
        <Label color={on ? t.text : t.textSecondary} size={13} lineHeight={18} weight={on ? 500 : 400}>{`Attempt ${a.attempt}`}</Label>
      </div>
    })}
  </div>
}

/** The selected job's steps, the failed or running one marked. */
function Steps({ job, split }: { job: WorkflowJob; split: boolean }) {
  const t = useTheme()
  return <div testId="actions-steps" style={{ ...column, width: split ? 288 : '100%', flexShrink: 0, ...(split ? { borderRightWidth: 1 } : { maxHeight: 220, borderBottomWidth: 1 }), borderColor: t.surfaceDivider }}>
    <div style={{ ...row, minHeight: 44, flexShrink: 0, paddingLeft: PAD, paddingRight: 12 }}>
      <div testId="actions-steps-job" style={{ display: 'flex', minWidth: 0 }}><Label color={t.text} size={14} lineHeight={20} weight={600} truncate>{job.name}</Label></div>
    </div>
    <div style={{ ...column, flexGrow: 1, minHeight: 0, overflowY: 'scroll', paddingLeft: 12, paddingRight: 12, paddingBottom: 12 }}>
      {job.steps.length === 0 && <Label color={t.textSecondary} size={13} lineHeight={20} style={{ paddingLeft: 12 }}>{isActive(job) ? 'Waiting for a runner.' : 'GitHub listed no steps for this job.'}</Label>}
      {job.steps.map(s => {
        const bucket = runBucket(s)
        const tint = bucket === 'failed' ? toneColor(t, 'danger') : bucket === 'running' ? toneColor(t, 'warning') : null
        const pending = bucket === 'queued' || bucket === 'skipped'
        const took = seconds(s.started_at, s.completed_at)
        return <div key={s.number} testId={`actions-step-${s.number}`} style={{ ...row, gap: 9, minHeight: 25, paddingLeft: 12, paddingRight: 10, borderRadius: RADIUS.sm, ...(tint ? { backgroundColor: alpha(tint, t.variant === 'dark' ? .12 : .1) } : {}) }}>
          <SwapOnChange kind="icon" active={bucket} style={{ width: 13, height: 13, flexShrink: 0 }}><RunGlyph item={s} size={13} /></SwapOnChange>
          <Label color={tint ?? (pending ? t.textTertiary : t.text)} size={13} lineHeight={18} weight={tint ? 600 : 400} truncate style={{ flexGrow: 1 }}>{s.name}</Label>
          {bucket === 'running'
            ? <LiveClock since={s.started_at} color={tint!} size={12} />
            : took !== null && !pending && <Label color={tint ?? t.textTertiary} size={12} lineHeight={16} style={{ flexShrink: 0 }}>{duration(took)}</Label>}
        </div>
      })}
    </div>
  </div>
}
