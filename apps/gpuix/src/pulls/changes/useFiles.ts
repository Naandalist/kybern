// The Changes tab's file list, loaded progressively: every page of `github.pr.page` (30 files each) is fetched in
// turn and only the file metadata is kept, so a long pull request costs a list of rows, not its whole diff. The
// last few pages stay in a small cache so the first selected file needs no second request; any other file's
// patch is loaded from its page when it is selected (`usePatch`).
import { asError } from '../../../../../packages/kybern-client/src/problem.ts'
import { useCallback, useEffect, useRef, useState } from 'react'
import type { PrFile } from '../../../../../packages/kybern-client/src/types.ts'
import { useApp } from '../../state.ts'
import { prCall, prClient } from '../client.ts'
import type { FileMeta } from './model.ts'

/** Pages fetched before pausing at a "Load more files" row (1,500 files). GitHub itself stops at 3,000. */
export const PAGES_PER_BURST = 50
const CACHED_PAGES = 3
const REMEMBERED_LISTS = 8
/** A burst ends before pages 51, 101, …: the list waits there for "Load more files". */
const atBurstEnd = (page: number) => page > 1 && (page - 1) % PAGES_PER_BURST === 0

export const toMeta = (f: PrFile, page: number): FileMeta => ({ path: f.path, old_path: f.old_path, status: f.status, additions: f.additions, deletions: f.deletions, page, patch_truncated: f.patch_truncated })

interface List { files: FileMeta[]; /** The next page to fetch; null once the last one arrived. */ resume: number | null; error?: Error }
/** Lists of the last few pull requests (metadata only), so returning to the tab shows rows at once. */
const lists = new Map<string, List>()
const pages = new Map<string, PrFile[]>()
const remember = <V,>(map: Map<string, V>, key: string, value: V, max: number) => { map.delete(key); map.set(key, value); while (map.size > max) map.delete(map.keys().next().value as string) }

async function fetchPage(projectId: string, number: number, repo: string | undefined, page: number) {
  return prCall(projectId, 'github.pr.page', { project_id: projectId, number, kind: 'files', page, ...(repo ? { repo } : {}) })
}

export function useChangedFiles(projectId: string, number: number, repo: string | undefined, revision: number, head: string | undefined, restoreThroughPage = 1) {
  const restorePage = Math.max(1, Math.min(100, restoreThroughPage))
  const connection = useApp(s => s.status)
  const client = prClient(projectId)
  const key = `${projectId}${repo ? `/${repo}` : ''}:${number}:${revision}:${head}`
  const [list, setList] = useState<{ key: string; value: List } | null>(null)
  const current = list?.key === key ? list.value : lists.get(key)
  const stop = useRef<() => void>(() => {})

  const run = useCallback((from: number, seed: FileMeta[]) => {
    stop.current()
    let canceled = false
    stop.current = () => { canceled = true }
    void (async () => {
      let files = seed
      for (let page = from; page === from || page <= restorePage || !atBurstEnd(page); page++) {
        let data
        try { data = await fetchPage(projectId, number, repo, page) } catch (e) {
          if (!canceled) { const value = { files, resume: page, error: asError(e) }; remember(lists, key, value, REMEMBERED_LISTS); setList({ key, value }) }
          return
        }
        if (canceled) return
        const raw = data.files as PrFile[]
        remember(pages, `${key}:${page}`, raw, CACHED_PAGES)
        // A path appears once: a page that shifted between requests must not repeat a row.
        const seen = new Set(files.map(f => f.path))
        files = [...files, ...raw.filter(f => !seen.has(f.path)).map(f => toMeta(f, page))]
        const last = !data.has_more || raw.length === 0
        const value = { files, resume: last ? null : page + 1 }
        remember(lists, key, value, REMEMBERED_LISTS)
        setList({ key, value })
        if (last) return
      }
    })()
  }, [projectId, number, repo, key, restorePage])

  useEffect(() => {
    if (!head || connection !== 'open') return
    const known = lists.get(key)
    // A finished list, or one waiting at a burst boundary, is shown as it is; anything else resumes where it stopped.
    if (known && (known.resume === null || (!known.error && atBurstEnd(known.resume) && known.resume > restorePage))) setList({ key, value: known })
    else run(known?.resume ?? 1, known?.files ?? [])
    return () => stop.current()
  }, [key, head, connection, run, client])

  const more = useCallback(() => { if (current && current.resume !== null) run(current.resume, current.files) }, [current, run])
  const files = current?.files ?? []
  return { files, loaded: !!current, done: current?.resume === null, paused: !!current && current.resume !== null && !current.error && atBurstEnd(current.resume), error: current?.error, more }
}

/** The patch of the selected file, from the page cache or loaded from its page. It stays once found, whatever the cache evicts. */
export function usePatch(projectId: string, number: number, repo: string | undefined, revision: number, head: string | undefined, file: FileMeta | undefined): { file: PrFile | undefined; loading: boolean } {
  const client = prClient(projectId)
  const key = `${projectId}${repo ? `/${repo}` : ''}:${number}:${revision}:${head}`
  const want = file ? `${key}:${file.path}` : ''
  const [found, setFound] = useState<{ want: string; file: PrFile | undefined } | null>(null)
  useEffect(() => {
    if (!file || !head) return
    const hit = pages.get(`${key}:${file.page}`)?.find(f => f.path === file.path)
    if (hit) { setFound({ want, file: hit }); return }
    let canceled = false
    void fetchPage(projectId, number, repo, file.page).then(data => {
      if (canceled) return
      const raw = data.files as PrFile[]
      remember(pages, `${key}:${file.page}`, raw, CACHED_PAGES)
      setFound({ want, file: raw.find(f => f.path === file.path) })
    }, () => { if (!canceled) setFound({ want, file: undefined }) })
    return () => { canceled = true }
  }, [want, head, projectId, number, repo, key, file?.page, client])
  const ready = found?.want === want
  return { file: ready ? found.file : undefined, loading: !!file && !ready }
}
