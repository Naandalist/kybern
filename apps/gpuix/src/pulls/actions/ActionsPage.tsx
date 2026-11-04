// The Actions tab of the Pull requests page (ADE-97, design "C · Pipeline"): the repository's workflow runs grouped by branch
// in the pull request list's column (the default branch pinned with a bead string per workflow, release tags included, then
// every branch, latest push first, with its pull request), filtered by status and workflow (read from GitHub) and by branch
// (as you type); the selected run, or the form that starts a workflow, beside it. Lists refresh every 10 s while a run in them is in progress, every 60 s otherwise, and not at
// all while the window is hidden (`refreshDelay`). Every list read also feeds the finished-run notices (`watch.ts`).
import { asError } from '../../../../../packages/kybern-client/src/problem.ts'
import { PlayIcon } from '@hugeicons/core-free-icons'
import { useGpuix } from '@gpuix/react'
import { Profiler, useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { isActive, refreshDelay, repoKey, type ActionsRepo } from '../../../../../packages/kybern-client/src/actions.ts'
import { filterGroups, groupRuns, usualDuration } from '../../../../../packages/kybern-client/src/actionsPipeline.ts'
import type { ActionsRunsResult, ActionsWorkflowsResult, Project } from '../../../../../packages/kybern-client/src/types.ts'
import { glyph, Icon } from '../../icons.tsx'
import { Button } from '../../kit/Button.tsx'
import { IconButton } from '../../kit/IconButton.tsx'
import { PickerChip } from '../../kit/PickerChip.tsx'
import { ResizeHandle, useResize } from '../../kit/Resize.tsx'
import { SearchField } from '../../kit/SearchField.tsx'
import { Segmented } from '../../kit/Segmented.tsx'
import { SpinIcon } from '../../kit/Spin.tsx'
import { SwapOnChange } from '../../motion/kit.tsx'
import { isWindowVisible, onWindowVisibility } from '../../native/visibility.ts'
import { N } from '../../nav/glyphs.ts'
import { Menu, MenuItem, MenuLabel, MenuSeparator } from '../../nav/overlay.tsx'
import { useNav } from '../../nav/store.ts'
import { useSpaceProjects } from '../../spaces/current.ts'
import { useApp } from '../../state.ts'
import { Label, useTheme } from '../../ui.tsx'
import { column, row } from '../../usage/controls.tsx'
import { prCall } from '../client.ts'
import { hasPullRequests, isGroup, listableMembers } from '../list/model.ts'
import { load, save } from '../persist.ts'
import { PrLoading, Problem } from '../PrStates.tsx'
import { FOLDER, HEADER_COMPACT_ACTIONS, PullsHeader } from '../PullsTabs.tsx'
import { PR_SCROLL_LAYOUT } from '../scroll-layout.ts'
import { Dispatch } from './Dispatch.tsx'
import { BEAD_PITCH } from './Beads.tsx'
import { RunDetail } from './RunDetail.tsx'
import { BranchesHeading, BranchItem, DefaultCard } from './RunList.tsx'
import { usePullsTab } from './store.ts'
import { observe } from './watch.ts'

type Status = '' | 'in_progress' | 'failure' | 'success'
const STATUSES = [['', 'All'], ['in_progress', 'Running'], ['failure', 'Failed'], ['success', 'Passed']] as const
/** Runs per read: one page of GitHub's, enough to fill the default branch's beads and the recent branches. */
const PAGE = 100
/** Runs one list shows at most (three pages); older ones are a filter away. */
const MAX_RUNS = 300
const REPO_KEY = 'pulls.actions.repo'
const PLAY = glyph(PlayIcon)
/** The triangle fills less of its box than the folder and refresh glyphs beside it; 18 pt matches their visual weight. */
const PLAY_SIZE = 18
/** A triangle's visual centre sits left of its box: 1 pt toward its point. */
const PLAY_NUDGE = { paddingLeft: 2 }

interface Choice extends ActionsRepo { name: string; project: Project }

/** Every repository the space can list runs for: single projects, and each member of a group project. */
function repoChoices(projects: readonly Project[]): Choice[] {
  return projects.filter(hasPullRequests).flatMap(p => isGroup(p)
    ? listableMembers(p).map(m => ({ projectId: p.id, repo: m.path, name: `${p.name} / ${m.name}`, project: p }))
    : [{ projectId: p.id, name: p.name, project: p }])
}

/** Whether the window is on screen, following AppKit's occlusion (and the other platforms' equivalents). */
export function useWindowVisible() {
  const [visible, setVisible] = useState(isWindowVisible)
  useEffect(() => onWindowVisibility(setVisible), [])
  return visible
}

/** `KYBERN_ACTIONS_PROFILE=1` (the performance check, `verify.actions-perf.ts`): each commit of the tab's tree on stderr. */
const PROFILE = process.env.KYBERN_ACTIONS_PROFILE === '1'
const profiled = (node: ReactNode) => PROFILE
  ? <Profiler id="actions" onRender={(_, phase, actual) => console.error(`[actions-commit] ${phase} ${actual.toFixed(2)}`)}>{node}</Profiler>
  : node

export function ActionsPage() {
  return profiled(<ActionsTab />)
}

function ActionsTab() {
  const t = useTheme()
  const { renderer } = useGpuix()
  const projects = useSpaceProjects()
  const connected = useApp(s => s.status === 'open')
  const selectedThread = useApp(s => s.threads.find(th => th.id === s.selectedId))
  const peek = useNav(s => s.pullsPeek)
  const focus = usePullsTab(s => s.focus)
  const focusSeq = usePullsTab(s => s.seq)
  const choices = useMemo(() => repoChoices(projects), [projects])
  const [repoId, setRepoId] = useState<string | null>(() => load<string | null>(REPO_KEY, null))
  const choice = choices.find(c => repoKey(c) === repoId) ?? choices.find(c => c.projectId === selectedThread?.project_id) ?? choices[0]
  const [workflow, setWorkflow] = useState('')
  const [status, setStatus] = useState<Status>('')
  const [branch, setBranch] = useState('')
  const [limit, setLimit] = useState(PAGE)
  const [revision, refresh] = useState(0)
  const [data, setData] = useState<{ key: string; result?: ActionsRunsResult; error?: Error } | null>(null)
  const [pending, setPending] = useState(false)
  const [workflows, setWorkflows] = useState<{ key: string; result: ActionsWorkflowsResult } | null>(null)
  const [current, setCurrent] = useState<{ key: string; branch: string | null } | null>(null)
  const [selected, setSelected] = useState<number | null>(null)
  const [dispatching, setDispatching] = useState(false)
  const [selectedActive, setSelectedActive] = useState(false)
  const visible = useWindowVisible()
  const list = useResize({ initial: 352, min: 304, max: 512, side: 'left', storageKey: 'kybern.pulls.listWidth' })
  const [windowWidth, setWindowWidth] = useState(() => renderer?.getWindowSize?.().width ?? 1280)
  const measure = () => { const size = renderer?.getWindowSize?.(); setWindowWidth(size?.width ?? 1280); if (size?.height) setWindowHeight(size.height) }
  const available = windowWidth - 60 - (peek ? 256 : 0)
  const wide = available >= 832
  const key = choice ? repoKey(choice) : ''
  const member = choice?.repo ? { repo: choice.repo } : {}
  const loadKey = `${key}|${workflow}|${status}|${limit}`

  const pick = (next: ActionsRepo) => {
    const id = repoKey(next)
    setRepoId(id); save(REPO_KEY, id)
    setWorkflow(''); setSelected(null); setDispatching(false); setLimit(PAGE)
  }
  // The PR page's checks and a finished-run notice open a repository, a branch or a run here.
  const lastFocus = useRef(0)
  useEffect(() => {
    if (!focus || lastFocus.current === focusSeq) return
    lastFocus.current = focusSeq
    pick(focus)
    setStatus(''); setBranch(focus.branch ?? '')
    setSelected(focus.runId ?? null)
  }, [focusSeq])

  useEffect(() => {
    if (!choice || !connected) return
    let canceled = false
    void prCall(choice.projectId, 'github.actions.workflows', { project_id: choice.projectId, ...member }).then(result => { if (!canceled) setWorkflows({ key, result }) }, () => {})
    void prCall(choice.projectId, 'git.branches', { project_id: choice.projectId, ...member }).then(r => { if (!canceled) setCurrent({ key, branch: r.current ?? null }) }, () => {})
    return () => { canceled = true }
  }, [key, connected])
  const ownBranch = current?.key === key ? current.branch : undefined

  useEffect(() => {
    if (!choice || !connected) return
    let canceled = false
    setPending(true)
    void prCall(choice.projectId, 'github.actions.runs', {
      project_id: choice.projectId, ...member, per_page: PAGE, pages: Math.ceil(Math.min(limit, MAX_RUNS) / PAGE), pulls: true,
      ...(workflow ? { workflow } : {}), ...(status ? { status } : {}),
    }).then(result => {
      if (canceled) return
      setData({ key: loadKey, result }); setPending(false)
      observe(choice, result.runs, result.viewer, ownBranch)
    }, e => { if (!canceled) { setData({ key: loadKey, error: asError(e) }); setPending(false) } })
    return () => { canceled = true }
  }, [loadKey, revision, connected])

  const shown = data?.key === loadKey ? data : null
  const runs = shown?.result?.runs ?? []
  const active = runs.some(isActive) || selectedActive
  // The refresh rule: 10 s with a run in progress on screen, 60 s otherwise, nothing while hidden; shown again, read at once.
  useEffect(() => {
    const delay = refreshDelay({ active, onScreen: true, windowVisible: visible })
    if (delay === null || !shown) return
    const timer = setTimeout(() => refresh(n => n + 1), delay)
    return () => clearTimeout(timer)
  }, [active, visible, shown])
  const wasVisible = useRef(visible)
  useEffect(() => { if (visible && !wasVisible.current) refresh(n => n + 1); wasVisible.current = visible }, [visible])

  const workflowList = workflows?.key === key ? workflows.result.workflows : []
  const workflowName = workflowList.find(w => String(w.id) === workflow)?.name ?? 'All workflows'
  const failure = shown?.error
  const open = useCallback((id: number) => { setSelected(id); setDispatching(false) }, [])
  const selectedRun = runs.find(r => r.id === selected)
  const detailWidth = available - (wide ? list.width : 0)
  const listWidth = wide ? list.width : available
  const defaultBranch = workflows?.key === key ? workflows.result.default_branch : undefined
  // As many beads as the column fits: the card's and a branch's workflow label, number and padding taken off.
  const beads = { main: Math.max(4, Math.floor((listWidth - 24 - 24 - 84 - 16 - 44) / BEAD_PITCH)), branch: Math.max(3, Math.floor((listWidth - 16 - 24 - 18 - 60 - 16 - 44) / BEAD_PITCH)) }
  const grouped = useMemo(() => groupRuns(runs, { ...(defaultBranch ? { defaultBranch } : {}), pulls: shown?.result?.pulls ?? [], beads }), [shown, defaultBranch, beads.main, beads.branch])
  const query = branch.trim()
  const branches = useMemo(() => filterGroups(grouped.branches, query), [grouped, query])
  const main = grouped.main && filterGroups([grouped.main], query).length ? grouped.main : undefined
  // A running run measures itself against the workflow's recent passing runs.
  const reference = selectedRun && runs.find(r => r.workflow_id === selectedRun.workflow_id && r.id !== selectedRun.id && r.status === 'completed' && r.conclusion === 'success')
  const usual = selectedRun ? usualDuration(runs, selectedRun.workflow_id, selectedRun.id) : null
  const filtered = !!(query || status || workflow)
  const [windowHeight, setWindowHeight] = useState(() => renderer?.getWindowSize?.().height ?? 800)
  const showList = wide || (selected === null && !dispatching)

  if (!choice) return <div testId="page-actions" style={{ ...column, flexGrow: 1 }}>
    <PullsHeader />
    <div style={{ ...column, gap: 10, padding: 24, paddingTop: 64 }}>
      <Label color={t.text} size={16} weight={500}>No GitHub projects</Label>
      <Label color={t.textSecondary} size={13}>Add a project with a GitHub remote to see its workflow runs here.</Label>
    </div>
  </div>

  return <div {...PR_SCROLL_LAYOUT} testId="page-actions" onMouseMove={measure} style={{ display: 'flex', flexGrow: 1, minHeight: 0, minWidth: 0 }}>
    {showList && <div style={{ ...column, width: wide ? list.width : '100%', flexShrink: 0, position: 'relative', borderRightWidth: wide ? 1 : 0, borderColor: t.border }}>
      <PullsHeader>
        <RepoPicker choices={choices} current={choice} onPick={pick} compact={listWidth < HEADER_COMPACT_ACTIONS} />
        <IconButton testId="actions-dispatch-open" icon={PLAY} glyph={PLAY_SIZE} style={PLAY_NUDGE} label="Run workflow" active={dispatching} onClick={() => { setDispatching(true); setSelected(null) }} />
        <IconButton testId="actions-refresh" icon={N.refresh} label="Refresh" disabled={pending} onClick={() => refresh(n => n + 1)}><SwapOnChange kind="icon" active={pending ? 'busy' : 'idle'} style={{ width: 16, height: 16 }}>{pending ? <SpinIcon source={N.refresh} size={16} color={t.iconSecondary} /> : <Icon source={N.refresh} size={16} color={t.iconSecondary} />}</SwapOnChange></IconButton>
      </PullsHeader>
      <div style={{ ...column, gap: 8, paddingLeft: 12, paddingRight: 12, paddingTop: 2, paddingBottom: 10 }}>
        <Segmented size="sm" fill aria-label="Status" testId="actions-status" value={status} options={STATUSES.map(([value, label]) => ({ value, label }))} onChange={v => { setStatus(v as Status); setLimit(PAGE) }} />
        <div style={{ ...row, gap: 6 }}>
          <div style={{ display: 'flex', flexShrink: 0 }}><Menu testId="actions-workflow" align="start" width={264} trigger={open => <PickerChip size="sm" open={open} truncate maxWidth={150} selected={!!workflow} label={`Workflow: ${workflowName}`}>{workflowName}</PickerChip>}>
            <MenuItem id="all" label="All workflows" onSelect={() => { setWorkflow(''); setLimit(PAGE) }} />
            {workflowList.length > 0 && <MenuSeparator />}
            {workflowList.map(w => <MenuItem key={w.id} id={String(w.id)} testId={`actions-workflow-${w.id}`} label={w.name} {...(w.state !== 'active' ? { detail: 'Off' } : {})} onSelect={() => { setWorkflow(String(w.id)); setLimit(PAGE) }} />)}
          </Menu></div>
          <div style={{ flexGrow: 1, minWidth: 0 }}>
            <SearchField label="Filter by branch" testId="actions-branch" value={branch} placeholder="Branch" width="100%" onChange={setBranch} />
          </div>
        </div>
      </div>
      <virtual-list {...PR_SCROLL_LAYOUT} testId="actions-list" estimatedItemHeight={96} style={{ flexGrow: 1, minHeight: 0, paddingTop: 2, paddingBottom: 12 }}>
        {failure ? <div key="error" style={{ padding: 20 }}><Problem error={failure} projectId={choice?.projectId} noun="workflow runs" refresh={() => refresh(n => n + 1)} /></div>
          : !shown ? <div key="loading" style={{ padding: 24 }}><PrLoading label="Loading workflow runs…" kind="list" rows={6} /></div>
          : <>
            {!main && branches.length === 0 && <div key="empty" style={{ ...column, gap: 10, padding: 24, paddingTop: 64 }}>
              <Label color={t.text} size={16} weight={500}>{filtered ? 'No matching runs' : 'No workflow runs yet'}</Label>
              <Label color={t.textSecondary} size={13} lineHeight={20}>{filtered ? 'Change or clear the filters to see more runs.' : `Runs of ${choice.name}'s GitHub Actions workflows appear here.`}</Label>
              {filtered && <Button size="sm" style={{ alignSelf: 'flex-start' }} onClick={() => { setBranch(''); setStatus(''); setWorkflow('') }}>Clear filters</Button>}
            </div>}
            {main && <DefaultCard key="default" group={main} selected={selected} onOpen={open} />}
            {branches.length > 0 && <BranchesHeading key="heading" count={branches.length} />}
            {branches.map((g, i) => <BranchItem key={`branch:${g.branch}`} group={g} selected={selected} last={i === branches.length - 1} onOpen={open} />)}
            {shown.result?.has_more && limit < MAX_RUNS && <div key="more" style={{ ...row, justifyContent: 'center', padding: 12 }}><Button size="sm" testId="actions-more" busy={pending} onClick={() => setLimit(l => Math.min(MAX_RUNS, l + PAGE))}>Show older runs</Button></div>}
            {shown.result?.has_more && limit >= MAX_RUNS && <div key="cap" style={{ padding: 16, paddingLeft: 20 }}><Label color={t.textTertiary} size={12} lineHeight={16}>Showing the latest 300 runs. Filter by workflow or status to find older ones.</Label></div>}
          </>}
      </virtual-list>
      {wide && <ResizeHandle testId="actions-list-resize" edge="right" label="Resize run list" dragging={list.dragging} onPointerDown={list.onPointerDown} onReset={list.reset}
        onKeyDown={e => { const step = e.modifiers?.shift ? 64 : 16; if (e.key === 'left') list.setWidth(list.width - step); else if (e.key === 'right') list.setWidth(list.width + step) }} />}
    </div>}
    {dispatching ? <Dispatch key={`dispatch-${key}`} target={choice} workflows={workflowList} defaultBranch={workflows?.key === key ? workflows.result.default_branch ?? '' : ''} ownBranch={ownBranch ?? null} initial={workflow} showBack={!wide}
        onClose={() => setDispatching(false)} onStarted={() => { setDispatching(false); refresh(n => n + 1) }} />
      : selected !== null ? <RunDetail key={`${key}:${selected}`} target={choice} runId={selected} listed={selectedRun} {...(reference ? { reference } : {})} usual={usual} width={detailWidth} height={windowHeight} showBack={!wide} visible={visible} onBack={() => setSelected(null)} onActive={setSelectedActive} onChanged={() => refresh(n => n + 1)} />
      : wide && <div style={{ ...column, flexGrow: 1, alignItems: 'center', justifyContent: 'center', gap: 16, padding: 24 }}>
        <Icon source={PLAY} size={24} color={t.textSecondary} />
        <Label color={t.text} size={16} weight={500}>Select a run</Label>
        <Label color={t.textSecondary} size={14} lineHeight={22} style={{ maxWidth: 320, textAlign: 'center' }}>Read its jobs and logs, run failed jobs again or send a failure to an agent.</Label>
      </div>}
  </div>
}

function RepoPicker({ choices, current, onPick, compact = false }: { choices: Choice[]; current: Choice; onPick: (c: ActionsRepo) => void; compact?: boolean }) {
  const t = useTheme()
  const groups = [...new Set(choices.filter(c => c.repo).map(c => c.projectId))]
  return <Menu testId="actions-repo" align="end" width={264} {...(compact ? { rootStyle: { flexShrink: 0 } } : {})} trigger={open => compact
    ? <IconButton icon={FOLDER} label={`Repository: ${current.name}`} active={open} />
    : <PickerChip size="sm" open={open} truncate maxWidth={150} label={`Repository: ${current.name}`}>{current.repo ? current.name.split(' / ').pop()! : current.name}</PickerChip>}>
    {choices.filter(c => !c.repo).map(c => <MenuItem key={repoKey(c)} id={repoKey(c)} testId={`actions-repo-${c.projectId}`} label={c.name} iconNode={<Lead on={repoKey(c) === repoKey(current)} color={t.link} />} onSelect={() => onPick(c)} />)}
    {groups.map(id => <div key={id} style={column}>
      <MenuSeparator />
      <MenuLabel>{choices.find(c => c.projectId === id)!.project.name}</MenuLabel>
      {choices.filter(c => c.projectId === id && c.repo).map(c => <MenuItem key={repoKey(c)} id={repoKey(c)} label={c.name.split(' / ').pop()!} iconNode={<Lead on={repoKey(c) === repoKey(current)} color={t.link} />} onSelect={() => onPick(c)} />)}
    </div>)}
  </Menu>
}

function Lead({ on, color }: { on: boolean; color: string }) {
  return <div style={{ width: 16, height: 16, display: 'flex', alignItems: 'center', justifyContent: 'center', flexShrink: 0 }}>{on && <Icon source={N.check} size={14} color={color} />}</div>
}
