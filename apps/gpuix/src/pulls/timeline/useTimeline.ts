import { useCallback, useEffect, useRef, useState } from 'react'
import { asError } from '../../../../../packages/kybern-client/src/problem.ts'
import type { PrTimelineEvent } from '../../../../../packages/kybern-client/src/types.ts'
import { useApp } from '../../state.ts'
import { prClient } from '../client.ts'
import { mergeTimeline, TIMELINE_LIMIT } from './model.ts'

const remembered = new Map<string, State & { until: number }>()
const remember = (state: State) => {
  if (!state.events.length || state.loading || state.error || remembered.get(state.key) === state) return
  remembered.delete(state.key)
  remembered.set(state.key, { ...state, until: Date.now() + 30_000 })
  while (remembered.size > 8) remembered.delete(remembered.keys().next().value!)
}

interface State { key: string; events: PrTimelineEvent[]; cursor?: string; page: number; hasMore: boolean; loading: boolean; error?: Error }

/**
 * The pull request's history, oldest first, one cursor page (30) at a time. A new key (another pull request, a refresh
 * or a new head) starts again; a reply for an older key is dropped. At most `TIMELINE_LIMIT` events are kept.
 */
export function useTimeline(projectId: string, number: number, repo: string | undefined, revision: number, head: string | undefined, restorePages = 1) {
  const connection = useApp(s => s.status)
  const client = prClient(projectId)
  const key = `${projectId}${repo ? `/${repo}` : ''}:${number}:${revision}:${head}:${connection}`
  const wantedPages = Math.max(1, Math.min(TIMELINE_LIMIT, restorePages))
  const [state, setState] = useState<State>(() => {
    const known = remembered.get(key)
    return known && known.until > Date.now() ? known : { key, events: [], page: 0, hasMore: false, loading: true }
  })
  const current = useRef(state)
  current.current = state
  useEffect(() => { remember(state) }, [state])
  const fetchPage = useCallback((forKey: string, cursor: string | undefined, page: number) => {
    if (!client) return
    void client.call('github.pr.page', { project_id: projectId, number, kind: 'timeline', page, ...(cursor ? { cursor } : {}), ...(repo ? { repo } : {}) }).then(result => {
      setState(s => s.key !== forKey ? s : { key: forKey, events: mergeTimeline(page === 1 ? [] : s.events, result.timeline ?? []), ...(result.next_cursor ? { cursor: result.next_cursor } : {}), page, hasMore: !!result.has_more && !!result.next_cursor, loading: false })
    }, e => { setState(s => s.key !== forKey ? s : { ...s, loading: false, error: asError(e) }) })
  }, [projectId, number, repo, client])
  useEffect(() => {
    if (!head || connection !== 'open') return
    const known = remembered.get(key)
    if (known && known.until > Date.now()) { setState(known); return }
    setState({ key, events: [], page: 0, hasMore: false, loading: true })
    fetchPage(key, undefined, 1)
  }, [key, head, connection, fetchPage])
  // A persisted reading anchor may refer to a later cursor page after the bounded cache expired or evicted it.
  // Rebuild that page extent before exposing the viewport to scroll restoration, bounded by events and page count.
  const restoring = state.key === key && state.page < wantedPages && state.hasMore && state.events.length < TIMELINE_LIMIT
  useEffect(() => {
    if (!restoring || state.loading || state.error || !state.cursor) return
    setState(s => ({ ...s, loading: true }))
    fetchPage(key, state.cursor, state.page + 1)
  }, [restoring, state.loading, state.error, state.cursor, state.page, key, fetchPage])
  const more = useCallback(() => {
    const s = current.current
    if (s.key !== key || s.loading || !s.hasMore || !s.cursor || s.events.length >= TIMELINE_LIMIT) return
    setState({ ...s, loading: true })
    fetchPage(key, s.cursor, s.page + 1)
  }, [key, fetchPage])
  const fresh = state.key === key
  return { events: fresh ? state.events : [], hasMore: fresh && state.hasMore, loading: !fresh || state.loading || (restoring && !state.error), restoring, page: fresh ? state.page : 0, error: fresh ? state.error : undefined, more, full: fresh && state.events.length >= TIMELINE_LIMIT }
}
