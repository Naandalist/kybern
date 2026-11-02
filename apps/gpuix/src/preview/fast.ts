// Fast mode of a forwarded preview (ADE-116). A dev server on another machine normally reaches the web view as
// `http://localhost:<port>`: HTTP/1.1, six connections, one request each, so a Vite page of hundreds of modules waits a
// round trip to that machine for every six. In Fast mode the daemon serves the forward as `https://localhost:<port>` with
// HTTP/2, and the whole page travels over one connection. The certificate is Kybern's own (per install); only Kybern's web
// views accept it, for that origin only.
//
// Known risk: a page that hard-codes `http://localhost:3000` for its API or an OAuth redirect sees a different origin over
// HTTPS (mixed content, cookie or redirect mismatches). Plain HTTP stays one click away in the address bar.
import type { PortForward } from '../../../../packages/kybern-client/src/types.ts'
import type { RemoteServer } from './remote.ts'

/** Above this round trip (ms) to the machine, Fast mode is on unless the user turned it off for the server. */
export const FAST_RTT_MS = 40

/** `plain` is the forward's `http://localhost` address, the one shown, copied and opened in a browser. */
export interface FastMode { on: boolean; plain: string; rttMs?: number }

/** Which server a hand-made choice belongs to. */
export const fastKey = (machine: Pick<RemoteServer, 'id' | 'port'>) => `${machine.id}:${machine.port}`

/** A choice made by hand wins; otherwise a measured round trip above `FAST_RTT_MS`. */
export function wantsFast(rttMs: number | undefined, choice: boolean | undefined): boolean {
  return choice ?? (rttMs !== undefined && rttMs > FAST_RTT_MS)
}

/** The page on the forward's HTTPS listener, or null while it has none. */
export function fastAddress(forward: Pick<PortForward, 'https_port'>, path = ''): string | null {
  if (!forward.https_port) return null
  return `https://localhost:${forward.https_port}${path.startsWith('/') || !path ? path : `/${path}`}`
}
