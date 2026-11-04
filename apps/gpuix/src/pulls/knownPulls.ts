import { classifyPrError } from '@/state/prReviewModel'
import { errorMessage } from '../../../../packages/kybern-client/src/problem.ts'
import { describeProblem } from '../../../../packages/kybern-client/src/problems.ts'
import { mapWithConcurrency } from '@/lib/workload'
import type { Project } from '../../../../packages/kybern-client/src/types.ts'
import type { prCall } from './client.ts'
import { hasPullRequests, type Row } from './list/model.ts'
import { githubPull, githubRepo, resolvePull, type KnownRepo, type PrTarget } from './tabTargets.ts'

/** All known network projects, independently of the active Space. Partial failures keep usable results. */
export async function loadKnownPulls(projects: readonly Project[], call: typeof prCall) {
  const loaded = await mapWithConcurrency(projects.filter(hasPullRequests), 3, async project => {
    const repos: KnownRepo[] = []
    const rows: Row[] = []
    const errors: string[] = []
    const add = (remote: string | null | undefined, repo?: string) => {
      const identity = githubRepo(remote)
      if (identity) repos.push({ projectId: project.id, ...(repo ? { repo } : {}), remote: identity })
    }
    if (project.kind !== 'group') add(project.repo?.remote)
    const results = await Promise.allSettled([
      call(project.id, 'github.pr.list', { project_id: project.id, state: 'all', limit: 100 }).then(result => {
        for (const pr of result.pull_requests) {
          rows.push({ project, pr })
          const identity = githubPull(pr.url)
          if (identity) add(`github.com/${identity.remote}`, pr.repo)
        }
        for (const member of result.repos ?? []) {
          if (member.error && !member.skipped) errors.push(`${project.name} / ${member.name}: ${member.error}`)
        }
      }),
      ...(project.kind === 'group' ? [call(project.id, 'projects.members.status', { project_id: project.id }).then(result => {
        for (const member of result.members) if (member.status === 'ok') add(member.remote_url, member.path)
      })] : []),
    ])
    for (const result of results) {
      if (result.status !== 'rejected' || classifyPrError(errorMessage(result.reason)) === 'no-remote') continue
      // The headline only ("Install GitHub CLI to load pull requests"): the raw text never reaches the page.
      errors.push(`${project.name}: ${describeProblem(result.reason, { action: 'load pull requests' }).title}`)
    }
    return { repos, rows, errors }
  })
  return { repos: loaded.flatMap(r => r.repos), rows: loaded.flatMap(r => r.rows), errors: loaded.flatMap(r => r.errors) }
}

/** A transcript URL usually resolves from repository identity without listing GitHub PRs across the network. */
export async function resolveKnownPull(url: string, projects: readonly Project[], call: typeof prCall): Promise<PrTarget | null> {
  const pull = githubPull(url)
  if (!pull) return null
  const direct = projects.find(project => project.kind !== 'group' && githubRepo(project.repo?.remote) === pull.remote)
  if (direct) return { projectId: direct.id, number: pull.number }
  const groups = projects.filter(project => project.kind === 'group' && [project.repo, ...(project.repos ?? [])].some(repo => githubRepo(repo?.remote) === pull.remote))
  if (groups.length) {
    const found = await mapWithConcurrency(groups, 3, async project => {
      try {
        const result = await call(project.id, 'projects.members.status', { project_id: project.id })
        return result.members.flatMap(member => member.status === 'ok' && githubRepo(member.remote_url) === pull.remote ? [{ projectId: project.id, repo: member.path, remote: pull.remote }] : [])
      } catch { return [] }
    })
    return resolvePull(url, found.flat())
  }
  // Older daemons may not have populated project identities yet. Listed URLs and member remotes remain registered targets.
  return resolvePull(url, (await loadKnownPulls(projects, call)).repos)
}
