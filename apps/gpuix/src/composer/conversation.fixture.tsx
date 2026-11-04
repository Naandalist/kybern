import '../native/local-build.ts'
import { render } from '@gpuix/react'
import { mkdirSync, writeFileSync } from 'node:fs'
import { PROTOCOL_VERSION, type ProviderStatus, type Thread, type UserMessage } from '../../../../packages/kybern-client/src/types.ts'
import { connect, getState, set, useApp, selectDraft } from '../state.ts'
import { ComposerView, Composer, useComposerOverlayHeight } from '../Composer.tsx'
import { Draft } from '../Draft.tsx'
import { ThemeProvider, Label, useTheme } from '../ui.tsx'
import { Button } from '../kit/Button.tsx'
import { onWindowKeyDown } from '../nav/keys.ts'
import { patch, restoreToComposer } from './store.ts'
import { messageText } from '../transcript/bubble.tsx'
import { useState } from 'react'
import { useGpuixRequired } from '@gpuix/react'
const at = new Date().toISOString()
const providers: ProviderStatus[] = ['claude-code', 'codex'].map(kind => ({ kind, display_name: kind, available: true, supported_permission_modes: ['supervised'], supports_fork: false, supports_model_switch: true, instances: ['default'], models: [{ id: kind + '-model', display_name: kind + ' model', is_default: true }] })) as ProviderStatus[]
let coordinator: Thread | null = null
let record: unknown = null
const project = { id: 'project', name: 'Fixture project', path: '/tmp', is_git: false, created_at: at, updated_at: at }
let uploads = 0
const server = Bun.serve({ hostname: '127.0.0.1', port: 0, async fetch(req, s) {
  // An attachment upload (`uploadBytes`): one asset per file.
  if (req.method === 'POST' && new URL(req.url).pathname === '/assets') { const bytes = await req.arrayBuffer(); return Response.json({ id: `asset-${++uploads}`, name: req.headers.get('x-kybern-filename') ?? 'file', media_type: req.headers.get('content-type') ?? 'application/octet-stream', size: bytes.byteLength }) }
  if (s.upgrade(req)) return; return new Response('fixture') }, websocket: { message(ws, raw) {
  const { id, method, params: p = {} } = JSON.parse(String(raw)); let result: unknown = {}
  switch (method) {
    case 'daemon.info': result = { protocol_version: PROTOCOL_VERSION, environment_id: 'p3-conversation', version: 'fixture' }; break
    case 'projects.list': result = { projects: [project] }; break
    case 'threads.list': result = { threads: coordinator ? [coordinator] : [] }; break
    case 'threads.get': result = { thread: coordinator, transcript: [], pending_approvals: [], pending_questions: [], notes: '' }; break
    case 'events.subscribe': result = { subscription_id: 'fixture', head_seq: 0 }; break
    case 'providers.list': result = { providers }; break
    case 'providers.accounts.list': result = { accounts: [] }; break
    case 'providers.accounts.catalog': result = providers.find(v => v.kind === p.provider.kind); break
    case 'settings.get': result = { providers: {}, default_provider: 'claude-code', default_permission_mode: 'supervised' }; break
    case 'queue.list': result = { messages: [] }; break
    case 'threads.target.get': result = { target: { provider: coordinator?.provider ?? providers[0], model: coordinator?.model } }; break
    case 'coordinator.get': result = coordinator; break
    case 'coordinator.open': coordinator = { id: 'coordinator', title: 'Project coordinator', project_id: project.id, coordinator_project_id: project.id, provider: p.provider, model: p.model, permission_mode: p.permission_mode, status: 'idle', created_at: at, updated_at: at, last_seq: 0 } as Thread; result = { thread: coordinator, created: true }; record = { method, params: p }; break
    case 'coordinator.switch_harness': coordinator = { ...coordinator!, provider: p.provider, model: p.model }; result = coordinator; record = { method, params: p }; break
    case 'skills.list': result = { skills: [{ name: 'Computer', display_name: 'Computer', path: 'kybern://computer', scope: 'plugin', enabled: true, description: 'Use your computer' }, { name: 'review-ui', display_name: 'Review UI', path: '/skills/review-ui/SKILL.md', scope: 'user', enabled: true, description: 'Review an interface change' }, { name: 'interfaces:better-layout', path: '/plugins/interfaces/skills/better-layout/SKILL.md', scope: 'app', enabled: true, description: 'Grouping, alignment and reading order' }, { name: 'treshnanda-taste', path: '/skills/treshnanda-taste/SKILL.md', scope: 'user', enabled: true, description: 'Personal taste profile' }] }; break
    case 'files.search': result = { files: process.env.KYBERN_FIXTURE_FILES?.split(',') ?? [] }; break
    case 'threads.search': result = { threads: [] }; break
    case 'notes.list': result = { notes: [{ id: 'note', title: 'Release notes', project_id: project.id, scope: 'project', updated_at: at, preview: '' }] }; break
    case 'tasks.items.list': result = { tasks: [{ id: 'task', key: 'KYB-1', title: 'Release checklist', project_id: project.id, scope: 'project', status: 'todo', updated_at: at, runs: [] }] }; break
  }
  ws.send(JSON.stringify({ jsonrpc: '2.0', id, result }))
  set({ statusDetail: `${method}:${Date.now()}` })
} } })
const dir = process.env.KYBERN_DATA_DIR
if (!dir) throw Error('An isolated fixture directory is required')
mkdirSync(dir, { recursive: true }); writeFileSync(dir + '/daemon.port', String(server.port)); writeFileSync(dir + '/daemon.token', 'fixture', { mode: 0o600 })
process.env.KYBERN_URL = `ws://127.0.0.1:${server.port}/ws`; process.env.KYBERN_TOKEN = 'fixture'
patch({ providers })
connect()
/** Test hook: a Finder drag that enters, hovers or leaves the composer card without dropping. */
function DragProbe() {
  const renderer = useGpuixRequired() as unknown as { simulateFileDrag(x: number, y: number, paths: string[], phase: string): void }
  const paths = [process.env.KYBERN_FIXTURE_DRAG_FILE ?? '/etc/hosts']
  return <div style={{ display: 'flex', gap: 8 }}>
    <Button testId="drag-enter" onClick={() => renderer.simulateFileDrag(300, 770, paths, 'enter')}>Drag enter</Button>
    <Button testId="drag-outside" onClick={() => renderer.simulateFileDrag(1000, 300, paths, 'move')}>Drag outside</Button>
    <Button testId="drag-exit" onClick={() => renderer.simulateFileDrag(300, 770, paths, 'exit')}>Drag exit</Button>
  </div>
}
function Fixture() {
  const t = useTheme(), [draft, setDraft] = useState(false), [sent, setSent] = useState<UserMessage | null>(null)
  const overlayHeight = useComposerOverlayHeight()
  const status = useApp(s => s.status), th = useApp(s => s.open.thread)
  useApp(s => s.statusDetail)
  return <div testId="conversation-ready" style={{ display: 'flex', flexDirection: 'column', width: '100%', height: '100%', backgroundColor: t.surface }}>
    <DragProbe />
    <Button testId="coordinator-draft" onClick={() => { selectDraft('project', 'coordinator'); setDraft(true) }}>Set up Coordinator</Button>
    <text testId="rpc-record" style={{ color: t.text }}>{JSON.stringify(record)}</text>
    <text testId="message-record" style={{ color: t.text }}>{JSON.stringify(sent)}</text>
    {/* A failed send's Edit (`SendFailed` in transcript/Turn.tsx) with the parts the composer just sent. */}
    <Button testId="restore-sent" onClick={() => { if (sent) restoreToComposer('conversation', sent.parts, messageText(sent.parts)) }}>Restore last send</Button>
    <text testId="overlay-height" style={{ color: t.text }}>{String(overlayHeight)}</text>
    <text testId="connected" style={{ color: t.text }}>{status}</text>
    {!draft && <div style={{ flexGrow: 1 }} />}
    {draft ? th ? <><div style={{ flexGrow: 1 }} /><Composer thread={th} /></> : <Draft /> : <ComposerView draftKey="conversation" projectId="project" provider={{ kind: 'claude-code', instance: 'default' }} providers={providers} accounts={[]} mode="supervised" onModeChange={() => {}} catalogLoading={false} onReloadModels={async () => {}} onSend={async message => { setSent(message) }} />}
  </div>
}
render(<ThemeProvider><Fixture /></ThemeProvider>, { title: 'Conversation verification', width: 1280, height: 820, focus: false, onKeyDown: onWindowKeyDown })
process.stdin.on('end', () => { server.stop(true); process.exit(0) })
