import { expect, test } from 'bun:test'
import type { PortForward } from '../../../../packages/kybern-client/src/types.ts'
import { addressPath, forwardedAddress, forwardPort, parseForwardTarget, type ForwardClient } from './remote.ts'
import { localServerOn } from './remote.ts'

const forward = (local: number): PortForward => ({ id: 'f', machine_id: 'm1', machine_name: 'os-kdi', remote_port: 8765, local_port: local, connections: 0, created_at: '2026-10-29T10:00:00Z' })

test('forwarding a port asks the daemon for that machine and port', async () => {
  const calls: unknown[] = []
  const client: ForwardClient = { call: async (method, params) => { calls.push([method, params]); return forward(41000) } }
  const result = await forwardPort(client, 'm1', 8765)
  expect(calls).toEqual([['ports.forward', { machine: 'm1', remote_port: 8765, origin: 'preview' }]])
  expect(forwardedAddress(result, '/app?x=1')).toBe('http://localhost:41000/app?x=1')
  expect(forwardedAddress(result)).toBe('http://localhost:41000')
})

test('"Forward a port…" asks for a manual forward', async () => {
  const calls: unknown[] = []
  const client: ForwardClient = { call: async (method, params) => { calls.push([method, params]); return forward(41000) } }
  await forwardPort(client, 'm1', 8765, 'manual')
  expect(calls).toEqual([['ports.forward', { machine: 'm1', remote_port: 8765, origin: 'manual' }]])
})

test('a forward error reaches the caller', async () => {
  const client: ForwardClient = { call: async () => { throw new Error('os-kdi is offline, so its ports cannot be forwarded.') } }
  await expect(forwardPort(client, 'm1', 1)).rejects.toThrow('offline')
})

test('the path of a server address is what follows the host', () => {
  expect(addressPath('http://localhost:5173')).toBe('')
  expect(addressPath('http://localhost:5173/')).toBe('')
  expect(addressPath('http://localhost:5173/a/b?c=1#d')).toBe('/a/b?c=1#d')
})

test('the forward field accepts a port, host:port or an address', () => {
  expect(parseForwardTarget('5173')).toEqual({ port: 5173, path: '' })
  expect(parseForwardTarget('localhost:3000')).toEqual({ port: 3000, path: '' })
  expect(parseForwardTarget('http://127.0.0.1:8080/docs')).toEqual({ port: 8080, path: '/docs' })
  expect(parseForwardTarget(' ')).toHaveProperty('error')
  expect(parseForwardTarget('0')).toHaveProperty('error')
  expect(parseForwardTarget('70000')).toHaveProperty('error')
  expect(parseForwardTarget('example.com:80')).toHaveProperty('error')
})

test('a thread on another machine forwards its own localhost servers and nothing else', () => {
  const box = { id: 'm-1', name: 'instabox' }
  expect(localServerOn('http://localhost:5173/app?x=1', box)).toEqual({ id: 'm-1', name: 'instabox', port: 5173, path: '/app?x=1', scheme: 'http' })
  expect(localServerOn('3000', box)).toEqual({ id: 'm-1', name: 'instabox', port: 3000, path: '', scheme: 'http' })
  expect(localServerOn('127.0.0.1:8080', box)?.port).toBe(8080)
  expect(localServerOn('https://localhost:8443', box)?.scheme).toBe('https')
  expect(localServerOn('https://example.com', box)).toBeUndefined()
  expect(localServerOn('/Users/me/index.html', box)).toBeUndefined()
  expect(localServerOn('http://localhost:5173', { ...box, offline: true })).toBeUndefined()
})
