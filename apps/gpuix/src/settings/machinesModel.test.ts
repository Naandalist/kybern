import { expect, test } from 'bun:test'
import type { KeepReachableStatus, MachineCard } from '../../../../packages/kybern-client/src/types.ts'
import type { MachineRow } from '../machines/index.ts'
import { footerText } from '../machines/format.ts'
import { buildCards, doctorRows, doctorSummary, isBehind, osName, outboxRows, reachableRow, unnamedMachines, updateResultRows, updateSummary } from './machinesModel.ts'

const row = (over: Partial<MachineRow> & { machineId: string; name: string }): MachineRow => ({ status: 'online', offline: false, isLocal: false, glyph: { letter: over.name[0]!, hue: 10 }, showGlyph: true, member: true, paused: false, ...over })
const member = (over: Partial<MachineCard> & { machine_id: string; name: string }): MachineCard => ({ os: 'linux', status: 'online', ...over })

test('versions compare by number, never by text', () => {
  expect(isBehind('0.18.2', '0.19.0')).toBe(true)
  expect(isBehind('0.9.0', '0.19.0')).toBe(true)
  expect(isBehind('0.19.0', '0.19.0')).toBe(false)
  expect(isBehind('0.20.0', '0.19.0')).toBe(false)
  expect(isBehind('0.19.0-rc1', '0.19.0')).toBe(false)
  expect(isBehind(undefined, '0.19.0')).toBe(false)
  expect(isBehind('dev', '0.19.0')).toBe(false)
})

test('OS names', () => {
  expect(osName('macos')).toBe('macOS')
  expect(osName('freebsd')).toBe('freebsd')
})

test('a card: status, facts, harnesses, relay name, and whether it is behind', () => {
  const cards = buildCards({
    rows: [row({ machineId: 'mac', name: 'This Mac', isLocal: true, version: '0.19.0' }), row({ machineId: 'vps', name: 'VPS', status: 'relaying', version: '0.18.0' }), row({ machineId: 'sat', name: 'satyamas', status: 'offline', offline: true, lastSeenAt: 0 })],
    members: [member({ machine_id: 'mac', name: 'Mac', os: 'macos' }), member({ machine_id: 'vps', name: 'VPS', ts_name: 'vps.tail.ts.net', relay_via: 'mac', harnesses: [{ kind: 'claude-code', available: true, signed_in: false }] }), member({ machine_id: 'sat', name: 'satyamas', os: 'windows', status: 'offline' })],
    infos: { sat: { cpu: { model: 'Ryzen', logical_cores: 12 } } },
    latest: '0.19.0',
    now: 3 * 3600_000,
  })
  const [mac, vps, sat] = cards
  expect([mac!.os, mac!.osLabel, mac!.version]).toEqual(['macos', 'macOS', '0.19.0'])
  expect(mac!.canPause).toBe(false)
  expect(mac!.behind).toBe(false)
  expect(vps!.status).toBe('Relaying via This Mac')
  expect([vps!.os, vps!.tsName, vps!.version]).toEqual(['linux', 'vps.tail.ts.net', '0.18.0'])
  expect(vps!.harnesses).toEqual([{ kind: 'claude-code', label: 'Claude', signedOut: true }])
  expect(vps!.behind).toBe(true)
  expect(sat!.status).toBe('Offline · last seen 3h ago')
  expect(sat!.specs).toEqual({ cpu: 'Ryzen', cores: '12 cores', memory: null, gpus: [] })
  expect(sat!.canPause).toBe(true)
})

test('a saved machine that is not in the network cannot be paused or renamed', () => {
  const [card] = buildCards({ rows: [row({ machineId: 'old', name: 'Old box', member: false })], members: [], infos: {} })
  expect([card!.canPause, card!.canRename, card!.member]).toEqual([false, false, false])
})

test('a card names the machine without .local and lists only installed harnesses', () => {
  const [card] = buildCards({
    rows: [row({ machineId: 'mac', name: 'Treshnandas-Macbook.local', isLocal: true })],
    members: [member({ machine_id: 'mac', name: 'Treshnandas-Macbook.local', os: 'macos' })],
    infos: { mac: { cpu: { model: 'Apple M1', logical_cores: 8 }, gpu: ['Apple M1'], mem: { total_bytes: 16 * 2 ** 30 }, harnesses: [{ kind: 'claude-code', available: true, signed_in: true, version: '2.1.312 (Claude Code)' }, { kind: 'codex', available: false }] } },
  })
  expect(card!.name).toBe('Treshnandas-Macbook')
  expect(card!.specs).toEqual({ cpu: 'Apple M1', cores: '8 cores', memory: '16 GB', gpus: [] })
  expect(card!.harnesses).toEqual([{ kind: 'claude-code', label: 'Claude 2.1.312', signedOut: false }])
})

test('update results list every machine with its reason', () => {
  const names = new Map([['b', 'satyamas'], ['c', 'vps']])
  const result = { started: ['b'], skipped: [{ machine_id: 'c', reason: 'Offline' }, { machine_id: 'x', reason: 'Already up to date' }] }
  expect(updateResultRows(result, names)).toEqual([
    { machineId: 'b', name: 'satyamas', started: true, text: 'Update started' },
    { machineId: 'c', name: 'vps', started: false, text: 'Offline' },
    { machineId: 'x', name: 'A machine', started: false, text: 'Already up to date' },
  ])
  expect(updateSummary(result)).toBe('Started 1 update, 2 skipped.')
  expect(updateSummary({ started: [], skipped: [{ machine_id: 'c', reason: 'Offline' }] })).toBe('Nothing was updated.')
  expect(updateSummary({ started: [], skipped: [] })).toBe('No other machines to update.')
})

test('the outbox lists only work that has not arrived, with the actions that make sense', () => {
  const names = new Map([['b', 'satyamas']])
  const item = (over: Record<string, unknown>) => ({ id: 'i', operation_id: 'o', target: 'b', kind: 'thread_send', payload: {}, state: 'queued', attempts: 0, created_at: '', expires_at: '', updated_at: '', ...over }) as never
  const rows = outboxRows([item({ id: '1' }), item({ id: '2', state: 'delivered' }), item({ id: '3', state: 'failed', kind: 'fork', last_error: 'No matching project', attempts: 2 }), item({ id: '4', state: 'expired', kind: 'settings_patch' }), item({ id: '5', state: 'sending' }), item({ id: '6', state: 'cancelled' })], names)
  expect(rows.map((row) => row.id)).toEqual(['1', '3', '4', '5'])
  expect(rows[0]).toEqual({ id: '1', title: 'Message', detail: 'Waiting for satyamas', canRetry: false, canCancel: true })
  expect(rows[1]).toEqual({ id: '3', title: 'Fork of a thread', detail: 'Did not reach satyamas: No matching project', canRetry: true, canCancel: true })
  expect(rows[2]!.detail).toBe('Not delivered to satyamas in time')
  expect(rows[3]).toMatchObject({ detail: 'Sending to satyamas', canCancel: false })
  expect(outboxRows([item({ id: '7', kind: 'move' })], names)[0]).toMatchObject({ title: 'Move of a thread', detail: 'Waiting for satyamas' })
})

test('only machines with a host-made name get the "Name your machines" hint', () => {
  const card = (name: string, over: object = {}) => ({ name, member: true, canRename: true, ...over })
  const cards = [card('srv1544252'), card('VM-3-60-ubuntu'), card('os-kdi'), card('ip-10-0-0-1', { member: false }), card('3f9a1c07', { canRename: false })]
  expect(unnamedMachines(cards).map((c) => c.name)).toEqual(['srv1544252', 'VM-3-60-ubuntu'])
})

test('a check run lists failures first with what to do, and says which machine a check is about', () => {
  const report = { generated_at: '2026-10-29T12:00:00Z', checks: [
    { id: 'tailscale.running', status: 'pass' as const, title: 'Tailscale is running' },
    { id: 'peer.reach', status: 'fail' as const, title: 'Cannot reach', next_step: 'Start Kybern on it', machine_id: 'pc' },
    { id: 'version', status: 'warn' as const, title: 'A machine is behind', next_step: 'Update all machines' },
  ] }
  const rows = doctorRows(report, new Map([['pc', 'satyamas']]))
  expect(rows.map((r) => [r.tone, r.title, r.next])).toEqual([
    ['bad', 'satyamas: Cannot reach', 'Start Kybern on it'],
    ['warn', 'A machine is behind', 'Update all machines'],
    ['good', 'Tailscale is running', ''],
  ])
  expect(doctorSummary(report)).toBe('2 of 3 checks need a look')
  expect(doctorSummary({ ...report, checks: [report.checks[0]!] })).toBe('All 1 check passed')
})

test('the reachable row explains the switch, shows a problem instead, and hides where nothing can be registered', () => {
  const on: KeepReachableStatus = { enabled: true, wanted: true, kind: 'task_scheduler', registered: true, running: false }
  expect(reachableRow(on)?.description).toContain('with the app closed')
  expect(reachableRow({ ...on, enabled: false, registered: false })?.description).toBe('Your other machines reach this one only while Kybern is open.')
  expect(reachableRow({ ...on, registered: false, error: 'Kybern could not register itself to start at login: Access is denied.' })).toEqual({ description: 'Kybern could not register itself to start at login: Access is denied.', problem: true, locked: false })
  expect(reachableRow({ ...on, external: 'The systemd user service kybernd.service already starts Kybern at login.' })?.locked).toBe(true)
  expect(reachableRow({ ...on, kind: null })).toBeNull()
  expect(reachableRow(undefined)).toBeNull()
})

// ADE-124: the daemon the user is at reached SatyaMas while this app's own connection to it was down, and the page
// said "Offline · last seen 10h ago" while the picker said "Online".
test('a machine the daemon reports online reads online, with a note while this computer is still reaching it', () => {
  const now = Date.parse('2026-11-03T03:05:00Z')
  const rows = [
    row({ machineId: 'mac', name: 'Mac', isLocal: true }),
    row({ machineId: 'sat', name: 'SatyaMas', status: 'offline', offline: true, lastSeenAt: now - 10 * 3600_000, version: '0.21.1' }),
    row({ machineId: 'box', name: 'Box', status: 'offline', offline: true, connecting: true }),
    row({ machineId: 'gone', name: 'Gone', status: 'offline', offline: true, lastSeenAt: now - 10 * 3600_000 }),
  ]
  const members = [
    member({ machine_id: 'mac', name: 'Mac' }),
    member({ machine_id: 'sat', name: 'SatyaMas', last_seen_at: '2026-11-03T03:04:00Z' }),
    member({ machine_id: 'box', name: 'Box' }),
    member({ machine_id: 'gone', name: 'Gone', status: 'offline', last_seen_at: '2026-11-03T02:05:00Z' }),
  ]
  const [, sat, box, gone] = buildCards({ rows, members, infos: {}, now })
  expect([sat!.status, sat!.tone, sat!.reaching, sat!.row.offline]).toEqual(['Online · not reachable from this computer yet', 'warn', true, false])
  expect(sat!.row.lastSeenAt).toBe(Date.parse('2026-11-03T03:04:00Z'))
  expect(box!.status).toBe('Online · connecting…')
  // Offline everywhere: last seen is the daemon's, which saw it an hour ago, not this app ten hours ago.
  expect([gone!.status, gone!.reaching]).toEqual(['Offline · last seen 1h ago', false])
})

// 2026-11-03: this app reached kdi-vps and SatyaMas, but the Mac daemon had no session to either; both read "Online".
test('a machine this app reaches while the daemon here has no session to it says the link is reconnecting', () => {
  const rows = [
    row({ machineId: 'mac', name: 'Mac', isLocal: true }),
    row({ machineId: 'kdi', name: 'kdi-vps' }),
    row({ machineId: 'sat', name: 'SatyaMas' }),
  ]
  const members = [
    member({ machine_id: 'mac', name: 'Mac', status: 'offline' }),
    member({ machine_id: 'kdi', name: 'kdi-vps', status: 'offline' }),
    member({ machine_id: 'sat', name: 'SatyaMas' }),
  ]
  const [mac, kdi, sat] = buildCards({ rows, members, infos: {} })
  expect([kdi!.status, kdi!.tone, kdi!.linkDown, kdi!.row.offline]).toEqual(["Online · this computer's link is reconnecting", 'warn', true, false])
  expect([sat!.status, sat!.linkDown]).toEqual(['Online', false])
  expect(mac!.linkDown).toBe(false)
})

test('the header counts offline machines of the network, not a saved one that is not in it', () => {
  const rows = [
    row({ machineId: 'mac', name: 'Mac', isLocal: true }),
    row({ machineId: 'sat', name: 'SatyaMas', status: 'offline', offline: true }),
    row({ machineId: 'old', name: 'This Mac', status: 'unpaired', offline: true, member: false }),
    row({ machineId: 'kdi', name: 'kdi', status: 'paused', offline: true, paused: true }),
  ]
  const cards = buildCards({ rows, members: [member({ machine_id: 'mac', name: 'Mac' }), member({ machine_id: 'sat', name: 'SatyaMas' })], infos: {} })
  expect(footerText(cards.map((card) => card.row))).toBe('4 machines · 1 paused')
  expect(footerText(rows)).toBe('4 machines · 1 offline · 1 paused')
})
