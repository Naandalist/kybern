/** Deterministic, in-memory RPC fixture. No GitHub or real daemon writes. */
import { createHash } from 'node:crypto'
import { existsSync, readFileSync, unlinkSync, writeFileSync, mkdtempSync, rmSync } from 'node:fs'
import { homedir, tmpdir } from 'node:os'
import path from 'node:path'
import { PROTOCOL_VERSION } from '../../../../packages/kybern-client/src/types.ts'
import { stateDetail, type PrState } from './verify.states.ts'
import { ACTION_PULLS, actionRun, actionRuns } from './verify.actions-fixture.ts'
import type { PrDetailResult, PrFile, PrReviewEntry, PrTimelineEvent, PullRequest, UsageRow } from '../../../../packages/kybern-client/src/types.ts'

export function actionFixture(options: { long?: boolean; horizontal?: boolean; picker?: boolean; paged?: boolean; slowFiles?: boolean; /** Extra running checks beyond the one in the base fixture. */ running?: number; /** A group project of three repositories (ADE-79): api and docs both have #108, web fails, infra is skipped. */ group?: boolean; /** ADE-85 list and Timeline: a group project of three repositories with pull requests in every readiness group, and a timeline history. */ readiness?: boolean; /** 65 distinct files in folders, with short patches (30 a page), for the Changes tab. */ changes?: boolean; /** A verdict state of the detail page (`verify.states.ts`). */ state?: PrState; /** ADE-97: 100 runs over 24 branches and a 40-job run, for the Actions performance check. */ actionsBig?: boolean; /** ADE-112: the linked thread is the project's Coordinator, so the dock has its Project panel. */ coordinator?: boolean } = {}) {
  const actionRunList = actionRuns(options.actionsBig)
  const scratch = mkdtempSync(path.join(tmpdir(), 'gpuix-scratch-pages-'))
  const now = new Date().toISOString()
  const members = (options.readiness ? READINESS_REPOS : ['api', 'web', 'docs', 'infra']).map(path => ({ path, name: path, status: 'ok' as const }))
  const project = options.group || options.readiness
    ? { id: 'pages-fixture', name: 'Acme', path: '/tmp/kybern-pages-fixture', is_git: false, kind: 'group' as const, members, created_at: now, updated_at: now }
    : { id: 'pages-fixture', name: 'Kybern fixture', path: '/tmp/kybern-pages-fixture', is_git: true, created_at: now, updated_at: now }
  const lead = options.coordinator ? { coordinator_project_id: project.id } : {}
  const thread = { id: 'linked', project_id: project.id, title: 'Linked conversation', provider: { kind: 'codex', instance: 'default' }, status: 'idle', permission_mode: 'supervised', last_seq: 0, created_at: now, updated_at: now, ...lead }
  if (options.horizontal) writeFileSync(path.join(scratch, 'gpuix-ui.json'), JSON.stringify({ 'kybern.split-view.v2': { version: 2, root: { kind: 'leaf', id: 'main', threadId: thread.id }, focusedPaneId: 'main', groups: { main: { threadId: thread.id, tabs: [{kind:'chat'}, {kind:'pr', projectId: project.id, number:108, ...(options.group ? {repo:'api'}:{})}], active: `pr:${project.id}${options.group ? '/api':''}:108`, preview: null } } } }))
  const subscribers = new Map<string, (event: object) => void>()
  let streamSeq=0
  const emit = (payload: object) => { for(const send of subscribers.values())send({thread_id:thread.id,turn_id:'stream-turn',seq:++streamSeq,at:new Date().toISOString(),...payload}) }
  const params: { method: string; params: Record<string, unknown> }[] = []
  const pr: PullRequest = { number: 108, title: 'Preserve reading position while streaming', url: 'https://example.com/pull/108', state: 'OPEN', head: 'fix/reading-position', base: 'main', is_draft: false, author: 'reviewer', updated_at: now, created_at: now, additions: 240, deletions: 28, mergeable: 'MERGEABLE', review_decision: 'APPROVED', checks_summary: { total: 3, passed: 2, failed: 0, pending: 1, skipped: 0 }, labels: [{ name: 'performance', color: '4285f4' }] }
  const detail: PrDetailResult = { pull_request: pr, body: '## Summary\n\nPreserve the reader’s position while a turn streams.\n\n- Stable rows keep their identity.\n- A bounded native list keeps old messages reachable.\n- Keyboard navigation remains immediate.\n\n### Verification\n\nChecked selection, wrapping, and follow behavior.', head_sha: '1234567890abcdef', reviewers: ['teammate'], checks: [{ name: 'Typecheck', status: 'COMPLETED', conclusion: 'SUCCESS', url: '' }, { name: 'Tests', status: 'COMPLETED', conclusion: 'SUCCESS', url: '' }, { name: 'Native visual verification', status: 'IN_PROGRESS', conclusion: '', url: '' }, ...Array.from({ length: options.running ?? 0 }, (_, i) => ({ name: `Running job ${i + 2}`, status: 'IN_PROGRESS', conclusion: '', url: '' }))], changed_files: 65, merge_state_status: 'CLEAN', reviews: [{ login: 'teammate', state: 'APPROVED' }], comment_count: 45, ...(options.readiness ? { timeline_counts: TIMELINE_COUNTS } : {}) }
  const files: PrFile[] = Array.from({ length: 30 }, (_, i) => ({ path: `src/components/Transcript${i}.tsx`, old_path: null, status: 'modified', additions: 1200, deletions: 1200, patch_truncated: false, patch: `@@ -1,1200 +1,1200 @@\n${Array.from({ length: 1200 }, (_, j) => `-const row${j} = renderOld(${j});\n+const row${j} = renderStable(${j});`).join('\n')}` }))
  // `changes`: distinct paths across the three pages, so the tree and paging can be checked.
  const folders = ['docs/platform-kb', 'docs/superpowers/specs', 'src/components/creator/instagram', 'src/components/landing', 'src/components/legal', 'src/lib', 'supabase/migrations', '.vitest/json']
  const changedFile = (i: number): PrFile => {
    const path = i === 5 ? 'docs/superpowers/specs/2026-10-24-expired-login-redirect-and-a-very-long-specification-name.md' : `${folders[i % folders.length]}/${['Ig', 'Nav', 'Cookie', 'legalVersions', 'output', 'get-new-fans', 'privacy'][i % 7]}${i}${['.tsx', '.ts', '.md', '.json', '.sql'][i % 5]}`
    return { path, old_path: null, status: 'modified', additions: 1 + (i * 7) % 40, deletions: (i * 3) % 12, patch_truncated: false, patch: `@@ -1,3 +1,4 @@\n const a${i} = 1\n-const b${i} = renderOld(${i})\n+const b${i} = renderStable(${i})\n+const c${i} = 3\n export {}` }
  }
  const entries: PrReviewEntry[] = Array.from({ length: 30 }, (_, i) => ({ id: i + 1, author: i % 2 ? 'teammate' : 'reviewer', body: `### Finding ${i + 1}\n\nKeep this row stable while streaming.\n\n\`row.id\` should survive a refresh.\n\n- Preserve focus\n- Preserve selection\n- Recheck wrapping`, state: i % 2 ? 'COMMENTED' : 'APPROVED', path: i % 3 ? null : 'src/components/Transcript.tsx', line: i % 3 ? null : i + 1, side: 'RIGHT', url: '', updated_at: now }))
  const usage = (key: string, n: number): UsageRow => ({ key, turns: n, usage: { input_tokens: n * 2400, output_tokens: n * 1200, cache_read_tokens: n * 8000, cache_write_tokens: n * 300 }, cost_usd: n * .52 })
  let problem: 'missing' | 'auth' | null = null
  const calls: string[] = []
  const actions: Record<string, unknown>[] = []
  let failAction = false
  let empty = false
  const server = Bun.serve({
    port: 0,
    hostname: '127.0.0.1',
    fetch(req, server) {
      // WebKit uses the browser session-ticket path; the in-memory fixture
      // accepts only its nonsecret local credential and never forwards it.
      const headers = { 'access-control-allow-origin': req.headers.get('origin') ?? '*', 'access-control-allow-headers': 'authorization,content-type', 'access-control-allow-methods': 'POST,OPTIONS', 'vary': 'Origin' }
      if (new URL(req.url).pathname === '/session') {
        if (req.method === 'OPTIONS') return new Response(null, { headers })
        if (req.method === 'POST' && req.headers.get('authorization') === 'Bearer local-fixture') return Response.json({ ticket: 'fixture-ticket' }, { headers })
        return new Response('Unauthorized', { status: 401, headers })
      }
      if (server.upgrade(req)) return
      return new Response('Not found', { status: 404 })
    },
    websocket: {
      message(ws, raw) {
        const message = JSON.parse(String(raw)) as { id: string; method: string; params: Record<string, unknown> }
        calls.push(message.method)
        params.push({ method: message.method, params: message.params })
        let result: unknown = {}
        let error: string | undefined
        switch (message.method) {
          case 'daemon.info': result = { protocol_version: PROTOCOL_VERSION, environment_id: 'pages-fixture', version: 'fixture', scopes: [] }; break
          case 'projects.list': result = { projects: [project, ...(options.picker ? [{...project,id:'other-known',kind:'single',members:[],is_git:true,name:'Other known project',space_id:'other-space',repo:{remote:'github.com/acme/other'}}]:[])] }; break
          case 'projects.members.status': result = { members: members.map(m=>({...m,dirty_files:0,remote_url:`git@github.com:acme/${m.path}.git`})), agents_md:false,claude_md:false }; break
          case 'threads.list': result = { threads: [{id:'linked',project_id:project.id,title:'Linked conversation',provider:{kind:'codex',instance:'default'},status:'idle',permission_mode:'supervised',worktree:{branch:pr.head,path:project.path},created_at:now,updated_at:now,...lead}], activity: [] }; break
          case 'notes.list': if (options.coordinator) result = { notes: [] }; else error = 'Fixture rejects notes.list'; break
          case 'threads.get': result = { thread, transcript: [], pending_approvals: [] }; break
          case 'queue.list': result = { messages: [] }; break
          case 'providers.list': result = { providers: [] }; break
          case 'providers.accounts.list': result = { accounts: [] }; break
          case 'settings.get': result = { default_provider: 'claude-code', default_permission_mode: 'supervised', providers: {} }; break
          case 'events.subscribe': { const id=`fixture-${message.id}`; result = { subscription_id:id,head_seq:0,replay_ready:true }; if(message.params.thread_id)subscribers.set(id,event=>ws.send(JSON.stringify({jsonrpc:'2.0',method:'event',params:{subscription_id:id,event}}))); break }
          case 'usage.limits': result = { providers: [], accounts: [], refreshing: [] }; break
          case 'usage.summary': {
            const rows = message.params.group_by === 'day' ? Array.from({ length: 30 }, (_, i) => { const d = new Date(); d.setUTCDate(d.getUTCDate() - 29 + i); return usage(d.toISOString().slice(0, 10), 5 + (i * 17) % 50) }) : [usage(message.params.group_by === 'model' ? 'claude-opus-5-5' : 'claude-code', 420), usage(message.params.group_by === 'model' ? 'gpt-6' : 'codex', 260)]
            result = { rows, total: usage('total', 680) }; break
          }
          case 'github.pr.list':
            if(options.picker && message.params.project_id==='other-known') {result={pull_requests:[{...pr,url:'https://github.com/acme/other/pull/108',title:'PR from another Space'}]};break}
            if (problem) error = problem === 'missing' ? 'gh: command not found' : 'authentication required; run gh auth login'
            else if(empty) result = {pull_requests:[]}
            else if (options.readiness) result = { pull_requests: readinessPulls(now, String(message.params.state ?? 'open')) }
            else if (options.group) result = {
              pull_requests: [
                { ...pr, repo: 'api', title: 'Add SSO to the API', ...(options.picker ? {url:'https://github.com/acme/api/pull/108'}:{}) },
                { ...pr, repo: 'api', number: 109, title: 'Rate-limit login', updated_at: new Date(Date.now() - 3600_000).toISOString() },
                { ...pr, repo: 'docs', title: 'Document SSO setup', ...(options.picker ? {url:'https://github.com/acme/docs/pull/108'}:{}), updated_at: new Date(Date.now() - 600_000).toISOString() },
              ],
              repos: [
                { repo: 'api', name: 'api' },
                { repo: 'web', name: 'web', error: 'HTTP 401: Bad credentials. Run gh auth login, then refresh.' },
                { repo: 'docs', name: 'docs' },
                { repo: 'infra', name: 'infra', error: 'No GitHub remote', skipped: true },
              ],
            }
            else result = { pull_requests: Array.from({ length: 30 }, (_, i) => ({ ...pr, number: 108 + i, title: i ? `Review change ${i}` : pr.title })) }
            break
          case 'github.pr.action': actions.push(message.params); if(failAction)error='Fixture action rejected';else result={message:'Done'};break
          case 'github.pr.detail': if ((options.group || options.readiness) && message.params.project_id === project.id && typeof message.params.repo !== 'string') { error = 'Acme has several repositories, so this pull request needs its repository.'; break }
            result = { ...detail, ...(options.long ? { body: Array.from({length: 40}, (_,i) => `### Verification ${i+1}\n\nPreserve **formatted content**, reading position and selection while scrolling.\n\n- Stable rows keep their identity.\n- The complete description remains reachable.\n\n\`row.id\` survives a refresh.`).join('\n\n') } : {}), pull_request: { ...pr, number: Number(message.params.number) } }; if (options.state) { const rich = stateDetail(options.state, result as PrDetailResult); result = { ...rich, pull_request: { ...rich.pull_request, number: Number(message.params.number) } } } break
          case 'github.pr.page': if (options.paged && message.params.kind === 'timeline') { const second=message.params.cursor==='page-2'; result={files:[],entries:[],page:second?2:1,has_more:!second,...(!second?{next_cursor:'page-2'}:{}),timeline:Array.from({length:30},(_,i)=>({id:`paged-${(second?30:0)+i}`,kind:'comment',created_at:now,author:'reviewer',body:`### Timeline item ${(second?30:0)+i+1}\n\nPreserve this later-page reading anchor.\n\n- Keep the selected history.\n- Rehydrate before restoring.`}))}; break }
            if (message.params.kind === 'timeline') { result = timelinePage(now, typeof message.params.cursor === 'string' ? message.params.cursor : undefined, options.long); break }
            result = { files: message.params.kind === 'files' ? (options.changes ? Array.from({ length: Math.max(0, Math.min(30, 65 - (((Number(message.params.page) || 1) - 1) * 30))) }, (_, i) => changedFile(((Number(message.params.page) || 1) - 1) * 30 + i)) : files) : [], entries: message.params.kind === 'files' ? [] : entries, page: message.params.page ?? 1, has_more: (Number(message.params.page) || 1) < 3 }; break
          // ADE-97: fake GitHub Actions. Writes are only recorded in `actions`.
          case 'github.actions.runs': {
            const status = message.params.status as string | undefined
            const runs = actionRunList.filter(r => !status || (status === 'in_progress' ? r.status !== 'completed' : r.conclusion === status)).filter(r => !message.params.workflow || String(r.workflow_id) === message.params.workflow).filter(r => !message.params.branch || r.head_branch === message.params.branch)
            result = { runs, total_count: runs.length, page: 1, has_more: false, viewer: 'ana', ...(message.params.pulls ? { pulls: ACTION_PULLS } : {}) }; break
          }
          case 'github.actions.run': result = actionRun(actionRunList, Number(message.params.run_id), message.params.attempt ? Number(message.params.attempt) : undefined); break
          case 'github.actions.log': { const offset = Math.min(Number(message.params.offset) || 0, ACTION_LOG.length); const limit = Number(message.params.limit) || 500; result = { available: true, lines: message.params.excerpt ? ACTION_LOG.slice(-40) : ACTION_LOG.slice(offset, offset + limit), offset: message.params.excerpt ? ACTION_LOG.length - 40 : offset, total_lines: ACTION_LOG.length, error_lines: [ACTION_LOG.length - 1] }; break }
          case 'github.actions.workflows': result = { workflows: [{ id: 1, name: 'CI', path: '.github/workflows/ci.yml', state: 'active' }, { id: 2, name: 'GPUIX platforms', path: '.github/workflows/gpuix-platforms.yml', state: 'active' }, { id: 3, name: 'Release', path: '.github/workflows/release.yml', state: 'active' }, { id: 4, name: 'Dist cache', path: '.github/workflows/dist-cache.yml', state: 'active' }], default_branch: 'main' }; break
          case 'github.actions.workflow': result = message.params.workflow === '2' ? { dispatchable: true, inputs: [{ name: 'checks', description: 'All, or a comma-separated list of checks', required: true, default: 'all', kind: 'string' }, { name: 'platform', kind: 'choice', options: ['all', 'windows', 'linux'], default: 'all' }, { name: 'verbose', kind: 'boolean', default: 'false' }] } : { dispatchable: false, inputs: [] }; break
          case 'github.actions.action': case 'github.actions.dispatch': actions.push({ method: message.method, ...message.params }); result = {}; break
          case 'git.branches': result = { current: 'fix/reading-position', branches: [] }; break
          default: if (!['events.unsubscribe', 'ping'].includes(message.method)) error = `Fixture rejects ${message.method}`
        }
        const response=JSON.stringify(error ? { jsonrpc: '2.0', id: message.id, error: { code: -32000, message: error } } : { jsonrpc: '2.0', id: message.id, result })
        if(options.slowFiles && message.method==='github.pr.page' && message.params.kind==='files' && Number(message.params.page)>1)setTimeout(()=>{try{ws.send(response)}catch{}},300)
        else ws.send(response)
      },
    },
  })
  const url = `ws://127.0.0.1:${server.port}/ws`
  const artifacts = [
    path.join(homedir(), 'Library/Application Support/kybern-gpuix', `workspace-${createHash('sha1').update(url).digest('hex').slice(0, 12)}.json`),
    path.join(homedir(), 'Library/Caches/kybern-gpuix', `providers-${url.replace(/[^a-z0-9]+/gi, '_')}.json`),
  ].map(file => ({ file, before: existsSync(file) ? readFileSync(file) : null }))
  return {
    env: { KYBERN_URL: url, KYBERN_TOKEN: 'local-fixture', GPUIX_BACKGROUND: '1', KYBERN_DATA_DIR: scratch, KYBERN_CLIENT_CONFIG_DIR: scratch, KYBERN_GPUIX_CREDENTIALS_DIR: scratch },
    stream: (delta: string) => emit({kind:'assistant_text_delta',message_id:'stream-message',origin:{kind:'root'},delta}), startStream: () => emit({kind:'turn_started',message_id:'stream-user',message:{parts:[{type:'text',text:'Keep reading the pull request while I stream.'}]}}),
    calls, params, actions, fail: (value:boolean)=>{failAction=value}, empty:(value:boolean)=>{empty=value},
    problem: (kind: typeof problem) => { problem = kind },
    close: () => {
      server.stop(true)
      rmSync(scratch, { recursive: true, force: true })
      // Call after closing the app so its deferred preferences cannot recreate files.
      for (const { file, before } of artifacts) {
        if (before) writeFileSync(file, before)
        else if (existsSync(file)) unlinkSync(file)
      }
    },
  }
}

// ---- ADE-97 Actions fixtures ----

export const ACTION_LOG = [
  '##[group]Run cargo test --workspace', 'cargo test --workspace', '##[endgroup]',
  ...Array.from({ length: 1200 }, (_, i) => `test orchestrator::tests::case_${i} ... \u001b[32mok\u001b[0m`),
  'failures:', '', '---- plugins::tests::fake_installers stdout ----', "thread 'plugins::tests::fake_installers' panicked at crates/kybern-daemon/src/plugins/tests.rs:286:9:", 'assertion `left == right` failed', '',
  'test result: \u001b[31mFAILED\u001b[0m. 1175 passed; 1 failed; 6 ignored', '\u001b[1m\u001b[91merror\u001b[0m: test failed, to rerun pass `-p kybern-daemon --lib`', '##[error]Process completed with exit code 101.',
]

// ---- ADE-85 list and Timeline fixtures (kept apart from the action fixture above) ----

export const READINESS_REPOS = ['core', 'tele', 'landing']
const TIMELINE_COUNTS = { comments: 2, reviews: 2, commits: 5 }
const ago = (now: string, minutes: number) => new Date(Date.parse(now) - minutes * 60_000).toISOString()

/** Open pull requests in every readiness group across the three repositories, plus a merged and a closed one. */
export function readinessPulls(now: string, state: string): PullRequest[] {
  const base: PullRequest = { number: 1, title: '', url: 'https://example.com/pull/1', state: 'OPEN', head: 'feature', base: 'main', is_draft: false, author: 'tresnanda', updated_at: now, created_at: now, additions: 12, deletions: 4, review_decision: 'REVIEW_REQUIRED', mergeable: 'MERGEABLE', merge_state_status: 'BLOCKED' }
  const open: PullRequest[] = [
    { ...base, repo: 'tele', number: 1190, title: 'fix(make-good): unpaid-link guard, creator notice, orphan Bunny cleanup (Grilling batch)', additions: 706, deletions: 8, checks_summary: { total: 9, passed: 4, failed: 2, pending: 0, skipped: 3 }, updated_at: ago(now, 4) },
    { ...base, repo: 'core', number: 2620, title: 'HOLD: Help docs for the public Rewards release (reward credits)', additions: 153, deletions: 30, review_decision: 'CHANGES_REQUESTED', updated_at: ago(now, 9) },
    { ...base, repo: 'tele', number: 1179, title: 'chore(deps): bump stripe from 20.4.1 to 23.0.0', additions: 7, deletions: 7, mergeable: 'CONFLICTING', merge_state_status: 'DIRTY', updated_at: ago(now, 15) },
    { ...base, repo: 'core', number: 2860, title: 'development -> staging', additions: 232, deletions: 53, checks_summary: { total: 12, passed: 1, failed: 0, pending: 6, skipped: 5 }, updated_at: ago(now, 20) },
    { ...base, repo: 'tele', number: 1182, title: 'chore(deps): bump @sentry/node from 10.66.0 to 11.8.0', additions: 564, deletions: 318, updated_at: ago(now, 31) },
    { ...base, repo: 'tele', number: 1181, title: 'chore(deps): bump ioredis from 5.10.1 to 6.0.0', additions: 48, deletions: 21, updated_at: ago(now, 40) },
    { ...base, repo: 'tele', number: 1178, title: 'chore(deps): bump the npm-minor-and-patch group with 5 updates', additions: 110, deletions: 162, review_decision: 'APPROVED', merge_state_status: 'CLEAN', updated_at: ago(now, 55) },
    { ...base, repo: 'landing', number: 412, title: 'chore(ci): bump actions/checkout from 4 to 5', additions: 6, deletions: 6, review_decision: '', merge_state_status: 'CLEAN', updated_at: ago(now, 70) },
    { ...base, repo: 'core', number: 2633, title: 'feat(rewards): credit ledger view for creators', additions: 412, deletions: 37, is_draft: true, merge_state_status: 'DRAFT', updated_at: ago(now, 90) },
  ]
  if (state === 'merged') return [{ ...base, repo: 'core', number: 2801, title: 'Release 2026-10-27', state: 'MERGED', additions: 88, deletions: 12, updated_at: ago(now, 600) }]
  if (state === 'closed') return [{ ...base, repo: 'tele', number: 1101, title: 'Try the new queue', state: 'CLOSED', additions: 3, deletions: 3, updated_at: ago(now, 900) }]
  return open
}

/** The history of #2860: two timeline pages (30 events would be a full page; the fixture keeps them short). */
export function timelinePage(now: string, cursor: string | undefined, long = false): { files: []; entries: []; page: number; has_more: boolean; timeline: PrTimelineEvent[]; next_cursor?: string } {
  const e = (id: string, minutes: number, over: Partial<PrTimelineEvent> & Pick<PrTimelineEvent, 'kind'>): PrTimelineEvent => ({ id, created_at: ago(now, minutes), author: 'tresnanda', ...over })
  const first: PrTimelineEvent[] = [
    e('c1', 120, { kind: 'commit', sha: 'a1f3c2e9d0b1', body: 'IG review: neutral content wording (#2858)' }),
    e('c2', 119, { kind: 'commit', sha: '7be019d44a10', body: 'docs(specs): PL-01..04 rows after the launch_config drop (#2856)' }),
    e('c3', 118, { kind: 'commit', sha: '5a38d8b0c2e7', body: "Merge branch 'main' into development" }),
    e('r1', 60, { kind: 'review', author: 'kevin', state: 'CHANGES_REQUESTED', body: 'The migration inserts the row, but nothing rolls it back on staging. Add a down step or say why it is safe.', inline_count: 2, inline_comments: [{ path: 'supabase/migrations/20261028024038_legal_versions_privacy_ig_content_wording.sql', line: 12, body: 'Needs `on conflict do nothing`: staging already has the 10e row.', url: 'https://example.com/c/1' }, { path: 'src/lib/legalVersions.ts', body: 'Outdated: this export moved.', url: 'https://example.com/c/2' }] }),
    e('m1', 40, { kind: 'comment', body: 'Rollback is the soft delete on `legal_versions`; added a note and the conflict guard.\n\n```sql\nupdate legal_versions set deleted_at = now() where slug = $1;\n```' }),
  ]
  const second: PrTimelineEvent[] = [
    e('f1', 12, { kind: 'force_push', sha: 'e9e5375a1b22', before_sha: '5a38d8b0c2e7' }),
    e('q1', 8, { kind: 'review_requested', target: 'kevin' }),
    e('c4', 7, { kind: 'commit', sha: 'e9e5375a1b22', body: 'Guard the insert and note the rollback' }),
    e('r2', 5, { kind: 'review', author: 'kevin', state: 'APPROVED', body: '' }),
    e('x1', 2, { kind: 'comment', author: 'github-actions', author_is_bot: true, body: 'Preview deployed.' }),
  ]
  // Scroll runs read a full page of long comments (the first one, `r1`, is selectable like the old first entry).
  if (long) return { files: [], entries: [], page: 1, has_more: false, timeline: Array.from({ length: 30 }, (_, i) => e(i === 0 ? 'r1' : `l${i}`, 300 - i, { kind: i === 0 ? 'review' : 'comment', author: i % 2 ? 'teammate' : 'reviewer', state: 'COMMENTED', body: `### Finding ${i + 1}\n\nKeep this row stable while streaming.\n\n\`row.id\` should survive a refresh.\n\n- Preserve focus\n- Preserve selection\n- Preserve reading position` })) }
  return cursor === 'page-2' ? { files: [], entries: [], page: 2, has_more: false, timeline: second } : { files: [], entries: [], page: 1, has_more: true, next_cursor: 'page-2', timeline: first }
}
