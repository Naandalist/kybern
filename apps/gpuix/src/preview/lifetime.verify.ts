// ADE-128: a closed browser tab's page never comes back. Run by lifetime.test.ts in its own process (module mocks).
import assert from 'node:assert/strict'
import { mock } from 'bun:test'
if (!process.env.KYBERN_DATA_DIR?.includes('gpuix-scratch')) throw Error('Scratch data required')
const environment = { id: 'local', local: true, name: 'Primary', dataDir: process.env.KYBERN_DATA_DIR! }
mock.module('../shell/environment.ts', () => ({ activeEnvironment: () => environment, environmentKey: () => environment.id, useEnvironment: () => environment, environmentEndpoint: async () => null }))
const forwards: { machine: string; origin?: string }[] = [], revoked: string[] = [], relaysClosed: string[] = []
let port = 41000, serial = 0, delayed: (() => void) | undefined
const client = { info: { machine_id: 'primary', environment_id: 'primary', hostname: 'Primary' }, endpoint: { url: 'ws://127.0.0.1:9999/ws' }, call: async (method: string, p: any) => {
  if (method === 'ports.forward') {
    forwards.push({ machine: p.machine, origin: p.origin })
    if (p.machine === 'slow') await new Promise<void>(resolve => { delayed = resolve })
    return { id: `f${port}`, local_port: ++port }
  }
  if (method === 'previews.close') { revoked.push(p.ticket); return {} }
  return { target: { kind: 'server', url: p.url, port: Number(new URL(p.url).port) }, ...(p.proxy ? { ticket: `t${++serial}`, path: `/preview-proxy/t${serial}/` } : {}) }
} }
const noop = () => {}
mock.module('../state.ts', () => ({ getClient: () => client, localClient: () => client, threadClient: () => client, projectClient: () => client, machineOfThreadId: () => undefined, callThread: (_id: unknown, method: string, params: unknown) => client.call(method, params), ownProjectId: (t: any) => t.ownProjectId ?? t.project_id, getState: () => ({ threads: [{ id: 'thread', cwd: '/scratch' }], projects: [] }), DEFAULT_PANE: 'main', createPane: noop, closePaneState: noop, focusPane: noop, getFocusedPaneId: () => 'main', getPane: () => undefined, getPaneState: () => undefined, getThreadOpen: () => undefined, selectPaneThread: noop, selectThread: async () => {}, setPane: noop, setPaneAlias: noop, subscribe: () => noop }))
mock.module('./browser.ts', () => ({ launchBrowser: async () => {} }))
mock.module('./relay.ts', () => ({ openRelay: async (url: string) => ({ url: 'http://127.0.0.1:49999/', close: () => { relaysClosed.push(url) } }) }))
const { openRemoteServer, previewState, setPreview, disposePreview, previewThreads, previewUses } = await import('./store.ts')
const { forwardInUse } = await import('./lifetime.ts')
const instabox = { id: 'instabox', name: 'instabox', port: 3000, path: '' }

// Every preview open asks for a preview forward; "Forward a port…" asks for a manual one.
await openRemoteServer('thread', instabox)
assert.equal(forwards.at(-1)?.origin, 'preview')
await openRemoteServer('other', instabox, false, 'manual')
assert.equal(forwards.at(-1)?.origin, 'manual')
disposePreview('other')

// A page through a relay and a ticket, with history, Fast and device choices, and a chosen machine.
environment.local = false
await openRemoteServer('thread', { ...instabox, port: 5173 })
setPreview('thread', { fastChoice: { 'instabox:5173': true }, serversOn: 'instabox', device: true, forwardOpen: true })
assert.equal(previewState('thread').entries.length, 2)
assert.ok(previewThreads().includes('thread'))
assert.ok(forwardInUse({ machine_id: 'instabox', remote_port: 3000, local_port: 1, https_port: undefined }, previewUses()), 'history keeps its forwards')

disposePreview('thread')
const fresh = previewState('thread')
assert.deepEqual(fresh.entries, [])
assert.equal(fresh.index, -1)
assert.equal(fresh.load.phase, 'idle')
assert.equal(fresh.fastChoice, undefined)
assert.equal(fresh.serversOn, undefined)
assert.equal(fresh.forwardOpen, undefined)
assert.equal(fresh.device, false)
assert.equal(fresh.attemptedAddress, undefined)
assert.equal(fresh.attemptedMachine, undefined)
assert.ok(!previewThreads().includes('thread'))
assert.deepEqual(revoked, ['t1'], 'the ticket is revoked')
assert.equal(relaysClosed.length, 1, 'the relay is closed')
assert.equal(forwardInUse({ machine_id: 'instabox', remote_port: 3000, local_port: 1, https_port: undefined }, previewUses()), false, 'nothing shows the forward any more')

// A page still opening when the tab closes never lands in the next tab.
environment.local = true
const late = openRemoteServer('thread', { ...instabox, id: 'slow' })
await new Promise(resolve => setImmediate(resolve))
disposePreview('thread')
delayed!(); assert.equal(await late, false)
assert.deepEqual(previewState('thread').entries, [])
assert.ok(!previewThreads().includes('thread'))
console.log('PASS closed tab disposes page, history, relay, ticket and choices; late opens dropped; forward origins')
