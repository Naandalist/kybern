// A dev server on another machine (ADE-86 U3): the Browser lists it under that machine, and opening it forwards its port to this
// machine (`ports.forward`) and shows `http://localhost:<local>`. The page, its assets and its hot-reload WebSocket are plain
// TCP to the webview, so nothing in the page changes.
import type { PortForward, PortsForwardParams } from '../../../../packages/kybern-client/src/types.ts'

/** A server on another machine, as an entry remembers it so reload, back and forward can forward the port again. */
export interface RemoteServer { id: string; name: string; port: number; path: string; scheme?: 'http' | 'https' }

export interface ForwardClient { call(method: 'ports.forward', params: PortsForwardParams): Promise<PortForward> }

/** Forward `port` of `machine`; the daemon returns the forward it already holds for the same machine and port. */
export const forwardPort = (client: ForwardClient, machineId: string, port: number): Promise<PortForward> => client.call('ports.forward', { machine: machineId, remote_port: port })

/** The page to show once the port is forwarded. */
export const forwardedAddress = (forward: Pick<PortForward, 'local_port'>, path = '', scheme = 'http'): string => `${scheme}://localhost:${forward.local_port}${path.startsWith('/') || !path ? path : `/${path}`}`

export const sourceAddress = (source: RemoteServer): string => forwardedAddress({ local_port: source.port }, source.path, source.scheme)

/** The part of a server's address after the host. */
export function addressPath(url: string): string {
  try { const parsed = new URL(url); return parsed.pathname === '/' && !parsed.search && !parsed.hash ? '' : `${parsed.pathname}${parsed.search}${parsed.hash}` } catch { return '' }
}

/** What the "Forward a port" field accepts: `5173`, `localhost:5173` or `http://localhost:5173/app`. */
export function parseForwardTarget(text: string): { port: number; path: string } | { error: string } {
  const trimmed = text.trim()
  if (!trimmed) return { error: 'Enter the port of the server, such as 5173.' }
  const absolute = /^\d+$/.test(trimmed) ? `http://localhost:${trimmed}` : trimmed.includes('://') ? trimmed : `http://${trimmed}`
  let url: URL
  try { url = new URL(absolute) } catch { return { error: 'Enter the port of the server, such as 5173 or localhost:5173.' } }
  if (!['localhost', '127.0.0.1', '[::1]'].includes(url.hostname)) return { error: "A forward reaches the other machine's own localhost, such as localhost:5173." }
  const port = Number(url.port || (url.protocol === 'https:' ? 443 : 80))
  if (!Number.isInteger(port) || port < 1 || port > 65535) return { error: 'Use a port between 1 and 65535.' }
  return { port, path: addressPath(url.toString()) }
}

/** A thread on another machine previews `raw` there: a dev server on that machine's own localhost becomes a forward of its
 *  port (direct TCP, kept-alive, Fast mode), not a page relayed through that machine's daemon. Anything else, or a machine
 *  that is offline, stays on the thread's own route (`undefined`). */
export function localServerOn(raw: string, machine: { id: string; name: string; offline?: boolean }): RemoteServer | undefined {
  if (machine.offline) return undefined
  const text = raw.trim()
  let url: URL
  try { url = new URL(/^\d+$/.test(text) ? `http://localhost:${text}` : text.includes('://') ? text : `http://${text}`) } catch { return undefined }
  if (!['localhost', '127.0.0.1', '[::1]'].includes(url.hostname) || !['http:', 'https:'].includes(url.protocol)) return undefined
  return { id: machine.id, name: machine.name, port: Number(url.port) || (url.protocol === 'https:' ? 443 : 80), path: addressPath(url.toString()), scheme: url.protocol === 'https:' ? 'https' : 'http' }
}
