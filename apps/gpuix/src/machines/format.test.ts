import { expect, test } from 'bun:test'
import { daemonLinkDown, footerText, lastSeen, shownStatus, statusText, statusTone } from './format.ts'

const now = Date.parse('2026-10-29T12:00:00Z')
test('the footer counts machines, the ones that are away, and queued work', () => {
  expect(footerText([{ offline: false }, { offline: true }, { offline: false }, { offline: false }])).toBe('4 machines · 1 offline')
  expect(footerText([{ offline: false }, { offline: false }])).toBe('2 machines')
  expect(footerText([{ offline: true }, { offline: false }], 2)).toBe('2 machines · 1 offline · 2 waiting')
  // Paused on purpose is not "offline".
  expect(footerText([{ offline: false }, { offline: true, paused: true }, { offline: false }, { offline: true }])).toBe('4 machines · 1 offline · 1 paused')
  expect(footerText([{ offline: false }, { offline: true, paused: true }])).toBe('2 machines · 1 paused')
})
test('offline machines say when they were last seen', () => {
  expect(statusText({ status: 'offline', lastSeenAt: now - 2 * 3600_000 }, now)).toBe('Offline · last seen 2h ago')
  expect(statusText({ status: 'offline', lastSeenAt: now - 10_000 }, now)).toBe('Offline · last seen just now')
  expect(statusText({ status: 'offline' }, now)).toBe('Offline · last seen unknown')
  expect(lastSeen(now - 3 * 86400_000, now)).toBe('last seen 3d ago')
})
test('relaying names the member, paused and unpaired say so', () => {
  expect(statusText({ status: 'relaying', via: 'VPS' })).toBe('Relaying via VPS')
  expect(statusText({ status: 'paused' })).toBe('Paused')
  expect(statusText({ status: 'unpaired', error: 'Pair this device with satyamas' })).toBe('Pair this device with satyamas')
  expect(statusText({ status: 'online' })).toBe('Online')
})
test('tones', () => {
  expect(['online', 'relaying', 'offline', 'paused', 'unpaired'].map((s) => statusTone(s as never))).toEqual(['good', 'warn', 'idle', 'idle', 'bad'])
})

// ADE-121: the daemon the user is at sees the machine online; the app's own connection is still coming back.
test('a machine the daemon reports online reads Online, whatever this connection says', () => {
  expect(shownStatus('offline', 'online')).toBe('online')
  expect(shownStatus('offline', 'offline')).toBe('offline')
  expect(shownStatus('offline', undefined)).toBe('offline')
  expect(shownStatus('paused', 'online')).toBe('paused')
  expect(shownStatus('relaying', 'online')).toBe('relaying')
})

// 2026-11-03: the picker said "Online" for kdi-vps and SatyaMas because this app reached them, while the Mac daemon's
// own sessions to both were down, so delegation, forwards and previews to them failed.
test('a machine this app reaches is not plainly online when the daemon here has no session to it', () => {
  expect(daemonLinkDown('online', 'offline')).toBe(true)
  expect(daemonLinkDown('online', 'online')).toBe(false)
  expect(daemonLinkDown('online', undefined)).toBe(false)
  expect(daemonLinkDown('online', 'offline', true)).toBe(false)
  expect(daemonLinkDown('paused', 'offline')).toBe(false)
  expect(daemonLinkDown('offline', 'offline')).toBe(false)
})
