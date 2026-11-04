import { useCallback, useEffect, useRef, useState } from 'react'
import { asError } from '../../../../packages/kybern-client/src/problem.ts'
import { afterReviewSubmission, assertReviewDraftHead, bindReviewDraft, readReviewDraft, repairReviewPrompt, type ReviewDraft } from '@/state/prReviewModel'
import type { MergeMethod, PrActionKind, PrDetailResult, PrPageKind, PrPageResult } from '../../../../packages/kybern-client/src/types.ts'
import { load, save as persist } from './persist.ts'
import { getData } from '../composer/store.ts'
import { openThread } from '../nav/history.ts'
import { refreshLists, useApp } from '../state.ts'
import { useMachines } from '../machines/index.ts'
import { prCall, prClient } from './client.ts'

const details = new Map<string, PrDetailResult>()

/** `repo`: the member repository of a group project the pull request is in. */
export function useReview(projectId: string, number: number, repo?: string) {
  const key = `pulls.draft.${projectId}${repo ? `.${repo}` : ''}.${number}`
  const member = repo ? { repo } : {}
  const connection = useApp(s => s.status)
  useMachines() // A remote project's reconnect retries its active review.
  const client = prClient(projectId)
  const detailKey = `${projectId}/${repo ?? ''}:${number}`
  const [detail, setDetail] = useState<PrDetailResult | null>(() => details.get(detailKey) ?? null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<Error | null>(null)
  const [revision, refresh] = useState(0)
  const [draft, setDraft] = useState(() => readReviewDraft(load(key, null)))
  const draftRef = useRef(draft)
  const [busy, setBusy] = useState(false)
  const submitting = useRef(false)
  const [actionError, setActionError] = useState<string | null>(null)
  const alive = useRef(true)
  useEffect(() => { alive.current = true; return () => { alive.current = false } }, [])
  const save = useCallback((next: ReviewDraft) => {
    draftRef.current = next
    // The shared shell store owns debouncing and environment scoping.
    persist(key, next)
    if (alive.current) setDraft(next)
  }, [key])
  const updateDraft = (patch: Partial<ReviewDraft>) => save(bindReviewDraft(draftRef.current, patch, detail?.head_sha ?? null))
  useEffect(() => {
    if (connection !== 'open') return
    let canceled = false
    setLoading(true)
    void prCall(projectId, 'github.pr.detail', { project_id: projectId, number, ...member }).then(next => {
      if (!canceled) {
        const known = details.get(detailKey)
        const value = known && JSON.stringify(known) === JSON.stringify(next) ? known : next
        details.delete(detailKey); details.set(detailKey, value)
        while (details.size > 8) details.delete(details.keys().next().value!)
        setDetail(value); setError(null); setLoading(false)
      }
    }, e => { if (!canceled) { setError(asError(e)); setLoading(false) } })
    return () => { canceled = true }
  }, [projectId, number, repo, revision, connection, client, detailKey])
  /** `options`: `merge` and `enable_auto_merge` take a method; `merge` may be an admin merge ("Merge anyway"). */
  const submit = async (action: PrActionKind, confirmedHead?: string, threadId?: string, options: { merge_method?: MergeMethod; admin?: boolean } = {}): Promise<boolean> => {
    if (!detail || submitting.current || !prClient(projectId)) return false
    const submitted = draftRef.current
    const review = ['comment', 'approve', 'request_changes'].includes(action)
    try {
      if (confirmedHead && confirmedHead !== detail.head_sha) throw new Error('The pull request changed while this confirmation was open. Cancel, review the current changes and open the action again.')
      if (review) assertReviewDraftHead(submitted, detail.head_sha)
      submitting.current = true; setBusy(true); setActionError(null)
      await prCall(projectId, 'github.pr.action', { project_id: projectId, number, action, head_sha: detail.head_sha, body: submitted.body, inline_comments: review ? submitted.inline : [], ...(threadId ? { thread_id: threadId } : {}), ...options, ...member })
      if (review) save(afterReviewSubmission(draftRef.current, submitted))
      if (alive.current) refresh(n => n + 1)
      return true
    } catch (e) { if (alive.current) setActionError(String(e)); return false }
    finally { submitting.current = false; if (alive.current) setBusy(false) }
  }
  /** `prompt`: an agent button's own text (fix a check, address feedback, resolve conflicts): it ignores the review draft and leaves it as it was. */
  const agentThread = useRef<string | undefined>(undefined)
  const repair = async (linkedId?: string, prompt?: string): Promise<boolean> => {
    const settings = getData().settings
    const client = prClient(projectId)
    if (!detail || !settings || !client || submitting.current) return false
    try {
      // Refused before a thread exists, so a member pull request leaves no orphan chat.
      if (repo) throw new Error("A pull request of a group project can't be checked out into a chat yet. Open that repository as its own project.")
      if (!prompt) assertReviewDraftHead(draftRef.current, detail.head_sha)
      const submitted = draftRef.current
      const text = prompt ?? repairReviewPrompt(number, detail.pull_request.title, detail.pull_request.url, detail.head_sha, submitted)
      submitting.current = true; setBusy(true); setActionError(null)
      let target = prompt ? linkedId ?? agentThread.current : submitted.threadId === 'new' ? undefined : submitted.threadId ?? linkedId
      if (!target) {
        const created = await client.call('threads.create', {
          project_id: projectId,
          provider: { kind: settings.default_provider, instance: 'default' },
          permission_mode: settings.default_permission_mode,
          use_worktree: true,
        })
        target = created.id
        // Preserve the created identity before checkout: retry reuses it.
        if (prompt) agentThread.current = target
        else save({ ...draftRef.current, threadId: target })
      }
      await client.call('github.pr.action', { project_id: projectId, number, action: 'checkout', thread_id: target, head_sha: detail.head_sha, for_repair: true, ...member })
      await client.call('threads.send', { thread_id: target, message: { parts: [{ type: 'text', text }] } })
      await refreshLists()
      openThread(target)
      return true
    } catch (e) { if (alive.current) setActionError(String(e)); return false }
    finally { submitting.current = false; if (alive.current) setBusy(false) }
  }
  return { detail, loading, error, refresh: () => refresh(n => n + 1), revision, draft, updateDraft, saveDraft: save, busy, actionError, submit, repair }
}

/** Page data is tagged by source and page so stale replies cannot replace a tab. */
export function usePage(projectId: string, number: number, kind: PrPageKind, page: number, revision: number, head: string | undefined, enabled = true, repo?: string) {
  const [state, setState] = useState<{ key: string; data?: PrPageResult; error?: string } | null>(null)
  const connection = useApp(s => s.status)
  const key = `${projectId}${repo ? `/${repo}` : ''}:${number}:${kind}:${page}:${revision}:${head}:${connection}`
  useEffect(() => {
    if (!enabled || !head || connection !== 'open') return
    let canceled = false
    void prCall(projectId, 'github.pr.page', { project_id: projectId, number, kind, page, ...(repo ? { repo } : {}) }).then(data => { if (!canceled) setState({ key, data }) }, e => { if (!canceled) setState({ key, error: String(e) }) })
    return () => { canceled = true }
  }, [key, projectId, number, repo, kind, page, enabled, head, connection])
  return { data: state?.key === key ? state.data : undefined, error: state?.key === key ? state.error : undefined }
}
export type Review = ReturnType<typeof useReview>
