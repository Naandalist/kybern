import { expect, test } from 'bun:test'
import type { PortForward, PortLoad } from '../../../../packages/kybern-client/src/types.ts'
import { amount, forwardOf, looksSlow, rate, routeText, slowLoadDetail, slowLoadText } from './slow.ts'

const load = (patch: Partial<PortLoad> = {}): PortLoad => ({ received_bytes: 4_200_000, bytes_per_second: 90_000, open_requests: 12, elapsed_ms: 4000, ...patch })

test('a page that is still fetching after three seconds says how far it got', () => {
  expect(slowLoadText(load({ expected_bytes: 31_000_000 }))).toBe('Loading over a slow connection — 4.2 of 31 MB · 90 KB/s')
  expect(slowLoadText(load())).toBe('Loading over a slow connection — 4.2 MB · 90 KB/s')
  expect(slowLoadText(load({ received_bytes: 300_000, cached_bytes: 500_000, bytes_per_second: 2_400_000, elapsed_ms: 12_000 }))).toBe('Loading over a slow connection — 800 KB · 2.4 MB/s')
})

test('a fast, finished or stalled-before-any-bytes load shows nothing', () => {
  expect(slowLoadText(undefined)).toBeNull()
  expect(slowLoadText(load({ elapsed_ms: 2500 }))).toBeNull()
  expect(slowLoadText(load({ open_requests: 0 }))).toBeNull()
  expect(slowLoadText(load({ received_bytes: 0 }))).toBeNull()
})

test('a load on a fast link shows nothing, unless much is left or it runs long', () => {
  expect(slowLoadText(load({ bytes_per_second: 5_000_000, expected_bytes: 31_000_000 }))).toBeNull()
  expect(slowLoadText(load({ bytes_per_second: 2_000_000 }))).toBeNull()
  expect(looksSlow(load({ bytes_per_second: 1_000_000, expected_bytes: 31_000_000 }))).toBe(true)
  expect(looksSlow(load({ bytes_per_second: 2_000_000, elapsed_ms: 12_000 }))).toBe(true)
  expect(looksSlow(load({ bytes_per_second: 90_000 }))).toBe(true)
})

test('only the figures change while the strip stays up', () => {
  expect(slowLoadDetail(load({ expected_bytes: 31_000_000 }))).toBe('4.2 of 31 MB · 90 KB/s')
  expect(slowLoadDetail(load({ received_bytes: 8_400_000, expected_bytes: 31_000_000, bytes_per_second: 120_000 }))).toBe('8.4 of 31 MB · 120 KB/s')
  expect(slowLoadDetail(undefined)).toBeNull()
})

test('amounts and rates read plainly', () => {
  expect(amount(31_000_000, 31_000_000)).toBe('31 MB')
  expect(amount(400)).toBe('1 KB')
  expect(rate(86_000)).toBe('86 KB/s')
})

test('the page finds its forward and the member it goes through', () => {
  const forward = { id: 'f', machine_id: 'instabox', machine_name: 'instabox-anjay', remote_port: 3000, local_port: 3000, connections: 1, created_at: '2026-11-03T00:00:00Z', route_via: { machine_id: 'kdi', machine_name: 'kdi-vps' } } satisfies PortForward
  expect(forwardOf([forward], { id: 'instabox', port: 3000 })).toBe(forward)
  expect(forwardOf([forward], { id: 'instabox', port: 5173 })).toBeUndefined()
  expect(forwardOf([forward], undefined)).toBeUndefined()
  expect(routeText(forward)).toBe('via kdi-vps')
  expect(routeText({ route_via: undefined })).toBeNull()
})
