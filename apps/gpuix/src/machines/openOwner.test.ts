// Opening a thread on another machine (ADE-124): a machine this app is not connected to is dialed and waited for,
// instead of the open failing at once with "This machine is offline".
import { expect, test } from 'bun:test'
import type { KybernClient } from '../../../../packages/kybern-client/src/client.ts'
import { OfflineError, UnknownMachineError } from '../../../../packages/kybern-client/src/machines.ts'
import { openOwner } from './openOwner.ts'

const client = (name: string) => ({ name }) as unknown as KybernClient
const local = client('local')

test('the machine the user is at opens on the app’s own client', async () => {
  expect(await openOwner(undefined, local, { clientFor: () => { throw new Error('not asked') }, reach: async () => { throw new Error('not asked') }, wanted: () => true })).toEqual({ kind: 'ready', owner: local })
})

test('a connected machine opens at once, without dialing', async () => {
  const pc = client('pc')
  let reached = 0
  const opened = openOwner('pc', local, { clientFor: () => pc, reach: async () => { reached++; return pc }, wanted: () => true })
  // Synchronously: an open that needs no wait keeps its timing (selection races depend on it).
  expect(opened).toEqual({ kind: 'ready', owner: pc })
  expect(reached).toBe(0)
})

test('a machine that is not connected is dialed, and the thread opens once it answers', async () => {
  const pc = client('pc')
  const steps: string[] = []
  const opened = await openOwner('pc', local, {
    clientFor: () => { throw new OfflineError('pc', true) },
    reach: async (id) => { steps.push(`reach ${id}`); return pc },
    wanted: () => true,
    waiting: () => steps.push('waiting'),
  })
  expect(steps).toEqual(['waiting', 'reach pc'])
  expect(opened).toEqual({ kind: 'ready', owner: pc })
})

test('a machine that still cannot be reached is offline, with the reason from the last try', async () => {
  const later = new OfflineError('pc', true, 'This machine is offline')
  const opened = await openOwner('pc', local, { clientFor: () => { throw new OfflineError('pc', true) }, reach: async () => { throw later }, wanted: () => true })
  expect(opened).toEqual({ kind: 'offline', error: later })
})

test('an answer after the user opened something else is dropped', async () => {
  let wanted = true
  const opened = await openOwner('pc', local, { clientFor: () => { throw new OfflineError('pc', true) }, reach: async () => { wanted = false; return client('pc') }, wanted: () => wanted })
  expect(opened).toEqual({ kind: 'gone' })
})

test('a machine this app does not know is not waited for', async () => {
  let reached = 0
  const unknown = new UnknownMachineError('nope')
  const opened = await openOwner('nope', local, { clientFor: () => { throw unknown }, reach: async () => { reached++; return local }, wanted: () => true })
  expect(opened).toEqual({ kind: 'offline', error: unknown })
  expect(reached).toBe(0)
})
