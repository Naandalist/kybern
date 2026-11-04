// The pull request's Changes tab: a file tree (or flat list) and the selected file's diff. The list fills the
// pane; pages of files load in the background (`useFiles.ts`), and closed folders persist per pull request
// (`model.ts`, `folders.ts`).
import { LinkSquare02Icon, ListViewIcon } from '@hugeicons/core-free-icons'
import { glyph, Icon } from '../../icons.tsx'
import { N } from '../../nav/glyphs.ts'
import { Tip } from '../../nav/overlay.tsx'
import { alpha } from '../../transcript/kit.tsx'
import { NativeDiffHeader } from '../../native/DiffView.tsx'
import { desktopDiffTheme } from '../../native/diff.ts'
import { diffFileIcons } from '../../native/diff-file-icons.ts'
import { parseUnifiedDiff } from '../../shared/lib/diff.ts'
import { getFileIconName } from '../../shared/lib/kit/fileIcons.ts'
import { Button } from '../../kit/Button.tsx'
import { IconButton } from '../../kit/IconButton.tsx'
import { SearchField } from '../../kit/SearchField.tsx'
import { TextArea } from '../../kit/TextArea.tsx'
import { memo, useCallback, useEffect, useMemo, useState } from 'react'
import { reviewDraftIsStale } from '@/state/prReviewModel'
import { prFileUrl } from '@/views/pullRequests/prDiff'
import { Label, ink, useTheme } from '../../ui.tsx'
import { column, row } from '../../usage/controls.tsx'
import { load, save } from '../persist.ts'
import { PR_SCROLL_LAYOUT } from '../scroll-layout.ts'
import { openUrl, PrLoading, Problem } from '../PrStates.tsx'
import type { Review } from '../useReview.ts'
import { filterFiles, flattenList, flattenTree, prKey, visibleFiles, type Row } from './model.ts'
import { useChangedFiles, usePatch } from './useFiles.ts'
import { readView, saveView } from '../viewState.ts'
import { useReadingPosition } from '../useReadingPosition.ts'
import { useClosedFolders } from './folders.ts'

const ROW_HEIGHT = 26
const INDENT = 14
const LIST_WIDTH = 320
const COMPACT_LIST_HEIGHT = 272
const MODE_KEY = 'pulls.changes.mode'

/** Tabular numerals for counts, stats and durations (the style type does not list the property). */
const num = (style: object = {}) => ({ ...style, fontVariantNumeric: 'tabular-nums' }) as never
const fmt = (n: number) => n.toLocaleString('en-US')


export function Changes({ projectId, number, repo, review, compact = false, viewKey }: { viewKey?: string; projectId: string; number: number; repo?: string; review: Review; compact?: boolean }) {
  const t = useTheme()
  const [path, setPath] = useState<string | null>(() => viewKey ? readView(viewKey).file : null)
  const diffScroll = useReadingPosition(viewKey, `diff:${path ?? ''}`)
  const [mode, setMode] = useState<'tree' | 'list'>(() => (load<string>(MODE_KEY, 'tree') === 'list' ? 'list' : 'tree'))
  const [query, setQuery] = useState(() => viewKey ? readView(viewKey).query : '')
  const [restorePage] = useState(() => viewKey ? readView(viewKey).filePage : 1)
  const list = useChangedFiles(projectId, number, repo, review.revision, review.detail?.head_sha, restorePage)
  const { files } = list
  const listVersion = `${review.revision}:${review.detail?.head_sha}`
  const [settledList, setSettledList] = useState<string | null>(null)
  useEffect(() => { if (list.loaded && (list.done || list.paused)) setSettledList(listVersion) }, [list.loaded, list.done, list.paused, listVersion])
  const listScroll = useReadingPosition(viewKey, 'files', list.loaded && (settledList === listVersion || list.done || list.paused))
  const key = prKey(projectId, number, repo)
  const folders = useClosedFolders(key)
  const collapsed = useMemo(() => new Set(folders.entry?.closed ?? []), [folders.entry?.closed])
  const filtering = query.trim() !== ''
  const rows = useMemo<Row[]>(() => {
    const shown = filterFiles(files, query)
    return mode === 'tree' ? flattenTree(shown, collapsed, filtering) : flattenList(shown)
  }, [files, query, mode, collapsed, filtering])
  const order = useMemo(() => visibleFiles(rows), [rows])
  /** Test ids count files only, so `pr-file-1` is the second file whatever the folders between. */
  const fileIndex = useMemo(() => new Map(order.map((f, i) => [f.path, i])), [order])

  // The first file in reading order is selected once, so more pages arriving never move the selection.
  useEffect(() => { if ((!path || (list.done && !files.some(f => f.path === path))) && order[0]) setPath(order[0].path) }, [path, order, files, list.done])
  const selected = files.find(f => f.path === path)
  const patchOf = usePatch(projectId, number, repo, review.revision, review.detail?.head_sha, selected)
  const file = patchOf.file
  const patch = useMemo(() => file ? `diff --git a/${file.old_path ?? file.path} b/${file.path}\n--- a/${file.old_path ?? file.path}\n+++ b/${file.path}\n${file.patch}` : '', [file])
  const pending = review.draft.pendingInline
  const stale = review.detail ? reviewDraftIsStale(review.draft, review.detail.head_sha) : false
  const [lineError, setLineError] = useState<string | null>(null)
  const [visibleLines, setVisibleLines] = useState(() => viewKey ? readView(viewKey).lines : 600)
  // The desktop table sizes its gutters from the currently expanded rows.
  // Native diff knows the full patch, so adjust its digit metric until expanded.
  const diffTheme = useMemo(() => {
    const theme = desktopDiffTheme(t, false)
    const lines = parseUnifiedDiff(patch).flatMap(f => f.hunks.flatMap(h => h.lines))
    const maxNumber = (rows: typeof lines) => rows.reduce((n, l) => Math.max(n, l.oldNo ?? 0, l.newNo ?? 0), 1)
    const digits = String(maxNumber(lines)).length
    const shownDigits = String(maxNumber(lines.slice(0, visibleLines))).length
    if (shownDigits < digits) theme.diff = { ...theme.diff, gutterDigitWidth: 13 * .62415 * shownDigits / digits }
    return theme
  }, [t, patch, visibleLines])

  useEffect(() => { if (viewKey) saveView(viewKey, { file: path, filePage: selected?.page ?? readView(viewKey).filePage, query, lines: visibleLines }) }, [viewKey, path, selected?.page, query, visibleLines])
  const select = useCallback((next: string) => { setPath(next); setVisibleLines(600) }, [])
  const toggleDir = useCallback((dir: string) => {
    const next = new Set(collapsed)
    if (!next.delete(dir)) next.add(dir)
    folders.close([...next])
  }, [collapsed, folders])
  const setModeSaved = (next: 'tree' | 'list') => { setMode(next); save(MODE_KEY, next) }

  const total = Math.max(review.detail?.changed_files ?? 0, files.length)

  return <div testId="pr-changes" style={{ ...column, flexGrow: 1, minHeight: 0, paddingTop: 16, paddingBottom: 16, paddingLeft: compact ? 14 : 24, paddingRight: compact ? 14 : 24, gap: 12 }}>
    {list.error && !files.length ? <Problem error={list.error} projectId={projectId} noun="this pull request" refresh={review.refresh} /> : <>
      <div style={{ display: 'flex', flexDirection: compact ? 'column' : 'row', flexGrow: 1, minHeight: 0, gap: 16 }}>
        <div testId="pr-files-column" style={{ ...column, ...(compact ? { width: '100%', height: COMPACT_LIST_HEIGHT } : { width: LIST_WIDTH }), flexShrink: 0, minHeight: 0, gap: 8 }}>
          <div style={{ ...column, gap: 8, paddingLeft: 6, paddingRight: 6, flexShrink: 0 }}>
            <div style={{ ...row, gap: 6, height: 24 }}>
              <Label color={t.text} size={12.5} lineHeight={16} weight={600} style={num({ flexGrow: 1 })}>{`${fmt(total)} ${total === 1 ? 'file' : 'files'}`}</Label>
              <Tip label="File tree" side="bottom" size={24}><IconButton testId="pr-mode-tree" icon={N.folder} size="sm" glyph={14} active={mode === 'tree'} label="Show as a tree" onClick={() => setModeSaved('tree')} /></Tip>
              <Tip label="Flat list" side="bottom" size={24}><IconButton testId="pr-mode-list" icon={glyph(ListViewIcon)} size="sm" glyph={14} active={mode === 'list'} label="Show as a list" onClick={() => setModeSaved('list')} /></Tip>
            </div>
            <SearchField testId="pr-files-filter" label="Filter files" placeholder="Filter files" value={query} onChange={setQuery} />
          </div>
          <virtual-list ref={listScroll} {...PR_SCROLL_LAYOUT} testId="pr-files" estimatedItemHeight={ROW_HEIGHT} style={{ flexGrow: 1, minHeight: 0, paddingLeft: 6, paddingRight: 6 }}>
            {rows.map(r => r.kind === 'dir'
              ? <DirRow key={`d:${r.key}`} row={r} onToggle={toggleDir} />
              : <FileRow key={`f:${r.key}`} index={fileIndex.get(r.key) ?? 0} row={r} tree={mode === 'tree'} selected={selected?.path === r.file.path} draft={review.draft.inline.some(c => c.path === r.file.path)} onSelect={select} />)}
            {!list.loaded && <div key="loading" style={{ padding: 8 }}><PrLoading label="Loading files…" rows={6} /></div>}
            {list.loaded && !rows.length && <div key="empty" style={{ padding: 8 }}><Label color={t.textSecondary} size={13}>{filtering ? 'No files match this filter.' : 'No changed files.'}</Label></div>}
            {list.loaded && !list.done && !list.paused && !list.error && <div key="more" style={{ ...row, height: ROW_HEIGHT, paddingLeft: 8 }}><Label color={t.textTertiary} size={12} style={num()}>{`Loading more files… ${fmt(files.length)} of ${fmt(total)}`}</Label></div>}
            {(list.paused || (list.error && files.length > 0)) && <div key="resume" style={{ ...column, gap: 6, padding: 8 }}>
              {list.error && <Label color={t.warning} size={12}>{`Could not load more files. ${list.error.message}`}</Label>}
              <Button testId="pr-files-more" size="sm" onClick={list.more}>{list.error ? 'Try again' : `Load more files (${fmt(files.length)} of ${fmt(total)})`}</Button>
            </div>}
          </virtual-list>
        </div>
        <div style={{ ...column, flexGrow: 1, minWidth: 0, minHeight: 0, borderWidth: selected ? 1 : 0, borderColor: t.border, borderRadius: 8, overflow: 'hidden' }}>
          {!selected ? (list.done ? <Label color={t.textSecondary} size={13}>No changed files.</Label> : list.paused ? <Label color={t.textSecondary} size={13}>Load more files to find the selected file.</Label> : <div style={{ padding: 16 }}><PrLoading label="Loading selected file…" rows={7} /></div>) : <>
            <div style={{ ...row, gap: 0, height: 37, flexShrink: 0, borderBottomWidth: 1, borderColor: t.border, backgroundColor: t.v('--background') }}>
              <div style={{ flexGrow: 1, minWidth: 0 }}><NativeDiffHeader file={{ ...selected, oldPath: selected.old_path }} /></div>
              <IconButton icon={glyph(LinkSquare02Icon)} size="sm" glyph={14} label="Open file on GitHub" onClick={() => { if (review.detail) void prFileUrl(review.detail.pull_request.url, selected.path).then(openUrl) }} />
            </div>
            {patchOf.loading ? <div style={{ padding: 16 }}><PrLoading label="Loading patch…" rows={5} /></div> : file?.patch ? <diff ref={diffScroll} key={file.path} {...PR_SCROLL_LAYOUT} testId="pr-native-diff" patch={patch} scroll theme={diffTheme} maxLines={visibleLines} onShowMore={() => setVisibleLines(n => n + 600)} onLineClick={e => {
              if (stale) { setLineError('This draft belongs to a different commit. Resolve the previous draft before choosing a new inline anchor.'); return }
              if (pending?.body.trim()) { setLineError('Add this comment to the review or cancel it before choosing another line.'); return }
              const line = e.newLine && e.newLine > 0 ? e.newLine : e.oldLine
              if (!line || line < 1) return
              setLineError(null)
              review.updateDraft({ pendingInline: { path: file.path, line, side: e.newLine && e.newLine > 0 ? 'RIGHT' : 'LEFT', body: '' } })
            }} style={{ userSelect: 'text', flexGrow: 1, minHeight: 100, minWidth: 0 }} /> : <Label color={t.textSecondary} size={12}>No patch available for this file.</Label>}
            {(file?.patch_truncated ?? selected.patch_truncated) && <Label color={t.textSecondary} size={11}>GitHub omitted or truncated this patch. Open the pull request on GitHub to read the full file.</Label>}
          </>}
        </div>
      </div>
    </>}
    {lineError && <Label color={t.warning} size={12}>{lineError}</Label>}
    {pending && <div style={{ ...column, gap: 8, padding: 10, borderRadius: 8, borderWidth: 1, borderColor: t.border }}><Label color={t.text} size={12}>{`Comment on line ${pending.line} · ${pending.side === 'LEFT' ? 'original' : 'new'} version · ${pending.path}`}</Label><TextArea label="Inline comment" testId="pr-inline-body" autoFocus minRows={2} maxRows={4} value={pending.body} onChange={v => review.updateDraft({ pendingInline: { ...pending, body: v.slice(0, 65536) } })} /><div style={{ ...row, justifyContent: 'flex-end', gap: 8 }}><Button variant="ghost" onClick={() => { review.updateDraft({ pendingInline: null }); setLineError(null) }}>Cancel</Button><Button testId="pr-inline-save" variant="primary" disabled={!pending.body.trim() || stale || review.draft.inline.length >= 100} onClick={() => { review.updateDraft({ inline: [...review.draft.inline, pending], pendingInline: null }); setLineError(null) }}>Add to review</Button></div></div>}
  </div>
}

const DirRow = memo(function DirRow({ row: r, onToggle }: { row: Extract<Row, { kind: 'dir' }>; onToggle: (key: string) => void }) {
  const t = useTheme()
  const flip = () => onToggle(r.key)
  return <Tip label={r.key} side="top" size={ROW_HEIGHT}>
    <div testId={`pr-dir-${r.key}`} role="button" aria-label={`${r.name}, ${r.open ? 'open' : 'closed'}`} aria-expanded={r.open} tabIndex={0} onClick={flip} onKeyDown={e => { if (e.key === 'enter' || e.key === 'space') flip() }} style={{ ...rowStyle, paddingLeft: 6 + r.depth * INDENT, hover: { backgroundColor: t.hover } }}>
      <Icon source={r.open ? N.chevronDown : N.chevronRight} size={12} color={t.textTertiary} />
      <Icon source={r.open ? N.folderOpen : N.folder} size={14} color={t.textSecondary} />
      <Label color={t.textSecondary} size={12.5} lineHeight={16} truncate style={{ flexGrow: 1 }}>{r.name}</Label>
    </div>
  </Tip>
})

const rowStyle = { display: 'flex', flexDirection: 'row', alignItems: 'center', gap: 6, height: ROW_HEIGHT, minWidth: 0, flexGrow: 1, paddingRight: 6, borderRadius: 7, cursor: 'pointer' } as const

const FileRow = memo(function FileRow({ row: r, index, tree, selected, draft, onSelect }: { row: Extract<Row, { kind: 'file' }>; index: number; tree: boolean; selected: boolean; draft: boolean; onSelect: (path: string) => void }) {
  const t = useTheme()
  const f = r.file
  const kind = getFileIconName(f.path)
  const choose = () => onSelect(f.path)
  return <Tip label={f.path} side="top" size={ROW_HEIGHT}>
    <div testId={`pr-file-${index}`} style={{ ...rowStyle, gap: 0, backgroundColor: selected ? ink(t, .07) : 'transparent', hover: { backgroundColor: selected ? ink(t, .07) : t.hover } }}>
      <div role="button" aria-label={f.path} tabIndex={0} onClick={choose} onKeyDown={e => { if (e.key === 'enter' || e.key === 'space') choose() }} style={{ display: 'flex', flexDirection: 'row', alignItems: 'center', gap: 6, flexGrow: 1, minWidth: 0, height: ROW_HEIGHT, paddingLeft: tree ? 6 + r.depth * INDENT + 18 : 8 }}>
        <Icon source={diffFileIcons[kind] ?? diffFileIcons['code-brackets']!} size={14} color={kind === 'react' ? '#61dafb' : t.textSecondary} />
        <Label color={t.text} size={12.5} lineHeight={16} truncate style={{ flexShrink: 1 }}>{r.name}</Label>
        {r.dir && <Label color={t.textTertiary} size={11.5} lineHeight={16} truncate style={{ flexShrink: 6, maxWidth: 96 }}>{r.dir}</Label>}
        <div style={{ flexGrow: 1 }} />
        {draft && <Label color={t.link} size={11} lineHeight={14} style={{ flexShrink: 0 }}>Draft</Label>}
        <div style={{ ...row, gap: 4, flexShrink: 0, minWidth: 56, justifyContent: 'flex-end' }}><Label color={t.green} size={12} lineHeight={16} style={num({ flexShrink: 0 })}>{`+${fmt(f.additions)}`}</Label><Label color={t.red} size={12} lineHeight={16} style={num({ flexShrink: 0 })}>{`−${fmt(f.deletions)}`}</Label></div>
      </div>
    </div>
  </Tip>
})
