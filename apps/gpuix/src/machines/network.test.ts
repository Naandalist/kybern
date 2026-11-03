import { expect, test } from 'bun:test'

// Keep the state/hub module seams in a separate process so their mocks cannot affect other GPUIX tests.
test('enable waits for a trailing status read, and switching primaries discards the old answer', () => {
  const result = Bun.spawnSync(['bun', '--eval', `
    import { expect, mock } from 'bun:test'
    let active, enabled = false, release, reads = 0
    const events = []
    const first = new Promise(resolve => { release = resolve })
    active = { status: 'open', call: async (method, params) => {
      if (method === 'network.set_enabled') { enabled = params.enabled; return {} }
      if (method === 'network.status') {
        reads++
        if (reads === 1) return first
        events.push('read-enabled')
        return { enabled }
      }
    } }
    mock.module('../state.ts', () => ({ getClient: () => active }))
    mock.module('./index.ts', () => ({ machineHub: () => ({ set: { syncMembers: async () => { events.push('sync') } } }) }))
    mock.module('./setupRuntime.ts', () => ({ startMachineSetup: () => events.push('start-setup'), cancelMachineSetup: () => events.push('cancel-setup') }))
    const { refreshNetwork, setNetworkEnabled, getNetwork, resetNetwork } = await import('./network.ts')
    const read = refreshNetwork()
    let done = false
    const turnOn = setNetworkEnabled(true).then(() => { done = true })
    await new Promise(resolve => setImmediate(resolve))
    expect(done).toBe(false)
    release({ enabled: false })
    await Promise.all([read, turnOn])
    expect(getNetwork().status.enabled).toBe(true)
    expect(events).toEqual(['start-setup', 'read-enabled', 'sync'])

    events.length = 0
    await setNetworkEnabled(false)
    expect(getNetwork().status.enabled).toBe(false)
    expect(events).toEqual(['cancel-setup', 'read-enabled', 'sync'])

    resetNetwork()
    let finishOld
    active = { status: 'open', call: () => new Promise(resolve => { finishOld = resolve }) }
    const old = refreshNetwork()
    active = { status: 'open', call: async () => ({ enabled: true, network_id: 'current' }) }
    const current = refreshNetwork()
    finishOld({ enabled: false, network_id: 'old' })
    await Promise.all([old, current])
    expect(getNetwork().status.network_id).toBe('current')
  `], { cwd: import.meta.dir, stdout: 'pipe', stderr: 'pipe' })
  expect(result.stderr.toString()).toBe('')
  expect(result.exitCode).toBe(0)
})

// `network.changed` carries the status: it is shown without another read, and a read sent before it cannot undo it.
test('a network.changed status is shown as is and beats a read that was already on its way', () => {
  const result = Bun.spawnSync(['bun', '--eval', `
    import { expect, mock } from 'bun:test'
    let release, reads = 0
    const active = { status: 'open', call: async (method) => { if (method === 'network.status') { reads++; return new Promise(resolve => { release = resolve }) } } }
    mock.module('../state.ts', () => ({ getClient: () => active }))
    mock.module('./index.ts', () => ({ machineHub: () => ({ set: { syncMembers: async () => {} } }) }))
    const { applyNetworkChanged, refreshNetwork, getNetwork } = await import('./network.ts')
    const pushed = { enabled: true, network_id: 'pushed', self_machine: { machine_id: 'a' }, members: [] }
    const read = refreshNetwork()
    expect(applyNetworkChanged(active, pushed)).toBe(true)
    expect(getNetwork().status.network_id).toBe('pushed')
    release({ enabled: true, network_id: 'older', self_machine: { machine_id: 'a' }, members: [] })
    await read
    expect(getNetwork().status.network_id).toBe('pushed')
    expect(reads).toBe(1)
    expect(applyNetworkChanged(active, {})).toBe(false)
    expect(applyNetworkChanged({ other: true }, { ...pushed, network_id: 'elsewhere' })).toBe(true)
    expect(getNetwork().status.network_id).toBe('pushed')
  `], { cwd: import.meta.dir, stdout: 'pipe', stderr: 'pipe' })
  expect(result.stderr.toString()).toBe('')
  expect(result.exitCode).toBe(0)
})
