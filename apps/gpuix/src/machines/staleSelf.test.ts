import { expect, test } from 'bun:test'
import { staleSelfRecords } from './staleSelf.ts'

const rec = (machineId: string, member: boolean, legacyProfileId?: string) => ({ record: { machineId, member, ...(legacyProfileId ? { legacyProfileId } : {}) } })

test('an older identity of this computer, saved under the same profile and not a member, is left out', () => {
  const connections = [rec('mac-now', true, 'local'), rec('mac-old', false, 'local'), rec('vps', true), rec('box', false, 'p-ssh')]
  expect([...staleSelfRecords(connections, 'mac-now')]).toEqual(['mac-old'])
})

test('nothing is left out without a local machine, a local profile, or for members and other profiles', () => {
  expect(staleSelfRecords([rec('mac-old', false, 'local')], null).size).toBe(0)
  expect(staleSelfRecords([rec('mac-now', true), rec('mac-old', false, 'local')], 'mac-now').size).toBe(0)
  expect(staleSelfRecords([rec('mac-now', true, 'local'), rec('twin', true, 'local'), rec('box', false, 'p-ssh')], 'mac-now').size).toBe(0)
})
