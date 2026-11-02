import { test, expect } from 'bun:test'
import { FAST_RTT_MS, fastAddress, fastKey, wantsFast } from './fast.ts'

test('Fast mode follows the round trip unless the user chose', () => {
  expect(wantsFast(undefined, undefined)).toBe(false)
  expect(wantsFast(FAST_RTT_MS, undefined)).toBe(false)
  expect(wantsFast(FAST_RTT_MS + 1, undefined)).toBe(true)
  expect(wantsFast(239, false)).toBe(false)
  expect(wantsFast(4, true)).toBe(true)
  expect(fastKey({ id: 'm-1', port: 5173 })).toBe('m-1:5173')
  expect(fastAddress({ https_port: 49152 }, '/app?x=1')).toBe('https://localhost:49152/app?x=1')
  expect(fastAddress({ https_port: 49152 }, 'app')).toBe('https://localhost:49152/app')
  expect(fastAddress({}, '/app')).toBeNull()
})
