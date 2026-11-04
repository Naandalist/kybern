import { openPullTab } from './openTab.ts'
import { targetOf } from './tabTargets.ts'
import { PR_SCROLL_LAYOUT } from './scroll-layout.ts'
import { Icon } from '../icons.tsx'
import { ResizeHandle, useResize } from '../kit/Resize.tsx'
import { SpinIcon } from '../kit/Spin.tsx'
import { N } from '../nav/glyphs.ts'
import { useGpuix, type PublicInstance } from '@gpuix/react'
import { useDeferredValue, useEffect, useMemo, useRef, useState } from 'react'
import { mapWithConcurrency } from '@/lib/workload'
import { classifyPrError, ghProblem, isGhSetup } from '@/state/prReviewModel'
import { errorMessage } from '../../../../packages/kybern-client/src/problem.ts'
import type { Project, PullRequest } from '../../../../packages/kybern-client/src/types.ts'
import { useApp } from '../state.ts'
import { prCall } from './client.ts'
import { useSpaceProjects } from '../spaces/current.ts'
import { addWindowKeyHandler } from '../nav/keys.ts'
import { useNav } from '../nav/store.ts'
import { Label, useTheme } from '../ui.tsx'
import { column, row } from '../usage/controls.tsx'
import { HEADER_COMPACT_PULLS, PullsHeader } from './PullsTabs.tsx'
import { usePullsTab } from './actions/store.ts'
import { ActionsPage } from './actions/ActionsPage.tsx'
import { Button } from '../kit/Button.tsx'
import { IconButton } from '../kit/IconButton.tsx'
import { SearchField } from '../kit/SearchField.tsx'
import { Segmented } from '../kit/Segmented.tsx'
import { Detail } from './PrDetail.tsx'
import { load, save } from './persist.ts'
import { PrListRow } from './list/PrListRow.tsx'
import { ProjectFilter } from './list/ProjectFilter.tsx'
import { RepoChips } from './list/RepoChips.tsx'
import { ALL_PROJECTS, groupRows, hasPullRequests, inScope, isGroup, memberName, readRepoMemory, rememberRepos, scopeTargets, type Row, type Scope } from './list/model.ts'
import { PrLoading, Problem } from './PrStates.tsx'
import { SwapOnChange } from '../motion/kit.tsx'
import { isPrimaryNotAltGr } from '../platform/keys.ts'

/** One project or member that didn't list: `text` names it for the partial list, `error` keeps its `Problem`. */
interface LoadError { text: string; error: unknown; projectId: string }
interface Loaded { rows: PullRequest[]; errors: LoadError[] }
type Filter = 'open' | 'merged' | 'closed' | 'all'
const FILTERS = [['open', 'Open'], ['merged', 'Merged'], ['closed', 'Closed'], ['all', 'All']] as const
const cache = new Map<string, { until: number; rows: PullRequest[]; errors: LoadError[] }>()
let lastSelection: { project: Project; number: number; repo?: string } | null = null
/** A group project's members can repeat a pull request number. */
const refOf = (projectId: string, number: number, repo?: string) => `${projectId}${repo ? `/${repo}` : ''}:${number}`
/** The repositories chosen per group project, kept across launches (`list/model.ts`, bounded). */
const REPO_MEMORY_KEY = 'pulls.repos'

/** The page's two tabs: pull requests, and the repository's Actions runs (ADE-97). */
export function PullRequestsPage() {
  const tab = usePullsTab(s => s.tab)
  return tab === 'actions' ? <ActionsPage /> : <PullRequestsList />
}

function PullRequestsList() {
  const t = useTheme()
  const { renderer } = useGpuix()
  const projects = useSpaceProjects()
  const connected = useApp(s => s.status === 'open')
  const peek = useNav(s => s.pullsPeek)
  const [filter, setFilter] = useState<Filter>('open')
  const [scope, setScope] = useState<Scope>(ALL_PROJECTS)
  const [remembered, setRemembered] = useState(() => readRepoMemory(load(REPO_MEMORY_KEY, {})))
  const [query, setQuery] = useState('')
  const deferredQuery = useDeferredValue(query)
  const [revision, refresh] = useState(0)
  const [loaded, setLoaded] = useState<{ scope: string; rows: Row[]; errors: LoadError[] } | null>(null)
  const [pending, setPending] = useState(false)
  const [selection, setSelection] = useState(lastSelection)
  const search = useRef<PublicInstance | null>(null)
  const [focus, setFocus] = useState<string | null>(null)
  // `PullRequests.tsx`: the list column resizes 304-512 (352 by default; below 304 the Actions header's controls no longer fit), saved, arrows 16px (shift 64px), double-click resets.
  const list = useResize({ initial: 352, min: 304, max: 512, side: 'left', storageKey: 'kybern.pulls.listWidth' })
  const [windowWidth, setWindowWidth] = useState(() => renderer?.getWindowSize?.().width ?? 1280)
  // Read geometry on user input, with no idle resize polling.
  const measure = () => setWindowWidth(renderer?.getWindowSize?.().width ?? 1280)
  const wide = windowWidth - 60 - (peek ? 256 : 0) >= 832
  const targets = useMemo(() => scopeTargets(projects, scope), [projects, scope])
  const loadScope = `${filter}:${targets.map(p => p.id).join(',')}`
  const scopeProject = scope.project ? projects.find(p => p.id === scope.project) : undefined
  // A group project remembers its repositories; the menu and the chips both go through here.
  const changeScope = (next: Scope) => {
    setScope(next)
    const group = next.project ? projects.find(p => p.id === next.project) : undefined
    if (group && isGroup(group)) {
      const memory = rememberRepos(remembered, group.id, next.repos)
      setRemembered(memory); save(REPO_MEMORY_KEY, memory)
    }
    setFocus(null)
  }
  useEffect(() => {
    if (!connected) return
    let canceled = false
    setPending(true)
    void mapWithConcurrency(targets, 3, async project => {
      if (canceled) return { rows: [] as Row[], errors: [] as LoadError[] }
      try {
        const key = `${project.id}:${filter}`
        const cached = cache.get(key)
        let loaded: Loaded
        if (revision === 0 && cached && cached.until > Date.now()) loaded = cached
        else {
          const result = await prCall(project.id, 'github.pr.list', { project_id: project.id, state: filter, limit: 30 })
          // A group project's members that couldn't be listed don't hide the others.
          const errors = (result.repos ?? []).flatMap(m => m.error && !m.skipped && classifyPrError(m.error) !== 'no-remote' ? [{ text: `${project.name} / ${m.name}: ${m.error}`, error: { message: m.error, data: m.problem ? { problem: m.problem } : undefined }, projectId: project.id }] : [])
          loaded = { rows: result.pull_requests, errors }
        }
        cache.set(key, { until: Date.now() + 30_000, ...loaded })
        if (cache.size > 100) cache.delete(cache.keys().next().value!)
        return { rows: loaded.rows.map(pr => ({ pr, project })), errors: loaded.errors }
      } catch (e) {
        const message = errorMessage(e)
        return { rows: [] as Row[], errors: classifyPrError(message) === 'no-remote' ? [] : [{ text: `${project.name}: ${message}`, error: e, projectId: project.id }] }
      }
    }).then(results => {
      if (canceled) return
      setLoaded({ scope: loadScope, rows: results.flatMap(r => r.rows).sort((a, b) => b.pr.updated_at.localeCompare(a.pr.updated_at)), errors: results.flatMap(r => r.errors) })
      setPending(false)
    }, e => { if (!canceled) { setLoaded({ scope: loadScope, rows: [], errors: [{ text: errorMessage(e), error: e, projectId: targets[0]?.id ?? '' }] }); setPending(false) } })
    return () => { canceled = true }
  }, [loadScope, filter, revision, connected])
  const data = loaded?.scope === loadScope ? loaded : null
  const matching = useMemo(() => {
    const q = deferredQuery.trim().toLowerCase().replace(/^#/, '')
    return (data?.rows ?? []).filter(row => inScope(row, scope)).filter(({ pr, project }) => !q || pr.title.toLowerCase().includes(q) || pr.head.toLowerCase().includes(q) || (memberName(project, pr.repo) ?? '').toLowerCase().includes(q) || String(pr.number).includes(q))
  }, [data, deferredQuery, scope])
  const groups = useMemo(() => groupRows(matching), [matching])
  const flat = useMemo(() => groups.flatMap(g => g.rows), [groups])
  // Open pull requests per project and repository, for the filter menu; only what the loaded scope covers is known.
  const counts = useMemo(() => {
    const m = new Map<string, number>()
    if (filter !== 'open') return m
    for (const { pr, project } of data?.rows ?? []) {
      m.set(project.id, (m.get(project.id) ?? 0) + 1)
      if (pr.repo) m.set(`${project.id}/${pr.repo}`, (m.get(`${project.id}/${pr.repo}`) ?? 0) + 1)
    }
    return m
  }, [data, filter])
  const open = (r: Row) => { const next = { project: r.project, number: r.pr.number, ...(r.pr.repo ? { repo: r.pr.repo } : {}) }; lastSelection = next; setSelection(next) }
  const back = () => { lastSelection = null; setSelection(null) }
  useEffect(() => addWindowKeyHandler(e => {
    if (isPrimaryNotAltGr(e.modifiers) && e.key === 'f') { if (!wide) back(); renderer?.focusElement?.(search.current?.id ?? 0); return true }
  }), [renderer, wide])
  const keyFor = (r: Row) => refOf(r.project.id, r.pr.number, r.pr.repo)
  const stateWord = filter === 'all' ? '' : `${filter} `
  // A gh problem replaces the list only when nothing listed: one member repository
  // signed out of an organization must not hide the others' pull requests.
  const failure = data?.rows.length ? undefined : data?.errors.find(e => isGhSetup(ghProblem(e.error))) ?? (data?.errors.length === 1 ? data.errors[0] : undefined)
  return <div {...PR_SCROLL_LAYOUT} testId="page-pulls" onMouseMove={measure} style={{ display: 'flex', flexGrow: 1, minHeight: 0, minWidth: 0 }}>
    {(wide || !selection) && <div style={{ ...column, width: wide ? list.width : '100%', flexShrink: 0, position: 'relative', borderRightWidth: wide ? 1 : 0, borderColor: t.border }}>
      <PullsHeader><ProjectFilter projects={projects.filter(hasPullRequests)} scope={scope} remembered={remembered} counts={counts} onChange={changeScope} compact={wide && list.width < HEADER_COMPACT_PULLS} /><IconButton testId="pr-refresh" icon={N.refresh} label="Refresh" disabled={pending} onClick={() => refresh(n => n + 1)}><SwapOnChange kind="icon" active={pending ? 'busy' : 'idle'} style={{ width: 16, height: 16 }}>{pending ? <SpinIcon testId="pr-refresh-spinner" source={N.refresh} size={16} color={t.iconSecondary}/> : <Icon source={N.refresh} size={16} color={t.iconSecondary}/>}</SwapOnChange></IconButton></PullsHeader>
      <div style={{ ...column, gap: 8, paddingLeft: 12, paddingRight: 12, paddingTop: 2, paddingBottom: 10 }}>
        {scopeProject && isGroup(scopeProject) && <RepoChips project={scopeProject} scope={scope} onChange={changeScope} />}
        <div style={{ ...row, gap: 6 }}>
          <Segmented size="sm" aria-label="State" value={filter} options={FILTERS.map(([value, label]) => ({ value, label, ...(value === filter && data && data.rows.length < 30 ? { count: data.rows.length } : {}) }))} onChange={v => { setFilter(v as Filter); setFocus(null) }} testId="pr-filter" />
          <div style={{ flexGrow: 1, minWidth: 0 }}><SearchField ref={search} label="Search pull requests" testId="pr-search" value={query} placeholder="Search" onChange={setQuery} width="100%" onKeyDown={e => { if (e.key === 'down' && flat[0]) { setFocus(keyFor(flat[0])); } }} /></div>
        </div>
      </div>
      <virtual-list {...PR_SCROLL_LAYOUT} testId="pr-list" estimatedItemHeight={72} style={{ flexGrow: 1, minHeight: 0 }}>
        {failure ? <div key="gh-error" style={{ padding: 20 }}><Problem error={failure.error} projectId={failure.projectId} refresh={() => refresh(n => n + 1)} /></div> : !data ? <div key="loading" style={{ padding: 24 }}><PrLoading label="Loading pull requests…" kind="list" rows={5} /></div> : <>
          {data.errors.length > 0 && <div key="errors" style={{ padding: 12 }}><Problem error={data.errors.map(e => e.text).join('\n')} partial={data.rows.length > 0} refresh={() => refresh(n => n + 1)} /></div>}
          {flat.length === 0 && <div key="empty" style={{ ...column, gap: 10, padding: 24, paddingTop: 64 }}><Label color={t.text} size={16} weight={500}>{!targets.length ? 'No GitHub projects' : query.trim() ? `No results for “${query.trim()}”` : `No ${stateWord}pull requests`}</Label><Label color={t.textSecondary} size={13}>{!targets.length ? 'Add a project with a GitHub remote to review its pull requests here.' : query.trim() ? '' : `Pull requests from ${scope.project ? targets[0]?.name : 'your GitHub projects'} appear here.`}</Label>{query.trim() && <Button size="sm" style={{ alignSelf: 'flex-start' }} onClick={() => setQuery('')}>Clear search</Button>}</div>}
          {groups.flatMap(group => [
            ...(group.label ? [<div key={`group-${group.group ?? 'done'}`} testId={`pr-group-${group.group ?? 'done'}`} style={{ ...row, gap: 6, height: 30, paddingLeft: 16, paddingRight: 8, paddingTop: 8 }}><Label color={t.textSecondary} size={12} lineHeight={16} weight={500}>{group.label}</Label><Label color={t.textTertiary} size={12} lineHeight={16}>{String(group.rows.length)}</Label></div>] : []),
            ...group.rows.map(r => <PrListRow key={keyFor(r)} item={r} selected={!!selection && refOf(selection.project.id, selection.number, selection.repo) === keyFor(r)} focused={focus === keyFor(r)} onOpen={() => open(r)} onOpenInTab={() => openPullTab(targetOf(r.project,r.pr))} onFocus={() => setFocus(keyFor(r))} onKey={key => {
              const at = flat.findIndex(x => keyFor(x) === keyFor(r))
              const next = key === 'down' ? Math.min(flat.length - 1, at + 1) : key === 'up' ? Math.max(0, at - 1) : key === 'home' ? 0 : key === 'end' ? flat.length - 1 : -1
              if (next >= 0) setFocus(keyFor(flat[next]!))
            }} />),
          ])}
        </>}
      </virtual-list>
      {wide && <ResizeHandle testId="pr-list-resize" edge="right" label="Resize pull request list" dragging={list.dragging} onPointerDown={list.onPointerDown} onReset={list.reset}
        onKeyDown={(e) => { const step = e.modifiers?.shift ? 64 : 16; if (e.key === 'left') list.setWidth(list.width - step); else if (e.key === 'right') list.setWidth(list.width + step) }} />}
    </div>}
    {selection ? <Detail key={refOf(selection.project.id, selection.number, selection.repo)} project={selection.project} number={selection.number} repo={selection.repo} onBack={back} showBack={!wide} width={windowWidth - 60 - (peek ? 256 : 0) - (wide ? list.width : 0)} onChanged={() => refresh(n => n + 1)} /> : wide && <div style={{ ...column, flexGrow: 1, alignItems: 'center', justifyContent: 'center', gap: 16, padding: 24 }}><Icon source={N.pullRequest} size={24} color={t.textSecondary}/><Label color={t.text} size={16} weight={500}>Select a pull request</Label><Label color={t.textSecondary} size={14} lineHeight={22} style={{maxWidth:300,textAlign:'center'}}>Read its summary, review changes and send findings to an agent.</Label><Label color={t.textSecondary} size={13}>↑ ↓ to move · Enter to open</Label></div>}
  </div>
}

