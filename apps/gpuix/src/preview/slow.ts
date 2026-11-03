// A forwarded page on a slow link (ADE-126). The daemon counts what each forward fetches for the page loading now
// (`PortForward.loading`, while one of its requests has been open two seconds; `ports.changed` repeats it every three
// seconds while it looks slow), so a page that keeps its content hidden until it has loaded looks slow instead of
// broken, and says which member its connections go through. A fast link shows nothing.
import type { PortForward, PortLoad } from '../../../../packages/kybern-client/src/types.ts'
import { shortMachineName } from '../../../../packages/kybern-client/src/forkMachine.ts'

/** A load shows as slow only after this long. */
export const SLOW_AFTER_MS = 3000
/** Below this rate a load is slow (bytes per second); the daemon's `load::SLOW_RATE`. */
export const SLOW_RATE = 500_000
/** A load expected to need longer than this, or running this long with no estimate, is slow too; `load::LONG_LOAD`. */
export const LONG_LOAD_MS = 10_000
/** What the strip says, the same for the whole load: the part a screen reader announces when it appears. */
export const SLOW_LOAD_MESSAGE = 'Loading over a slow connection'

/** The forward that serves a page from another machine. */
export function forwardOf(forwards: readonly PortForward[], machine: { id: string; port: number } | undefined): PortForward | undefined {
  return machine ? forwards.find(f => f.machine_id === machine.id && f.remote_port === machine.port) : undefined
}

const mb = (bytes: number) => { const value = bytes / 1e6; return value >= 10 ? Math.round(value).toString() : value.toFixed(1) }

/** `4.2 of 31 MB`, `850 KB`, `4.2 MB`. */
export function amount(done: number, total?: number): string {
  if (total && total > done) return `${mb(done)} of ${mb(total)} MB`
  return done >= 1e6 ? `${mb(done)} MB` : `${Math.max(1, Math.round(done / 1e3))} KB`
}

/** `90 KB/s`, `2.4 MB/s`. */
export function rate(bytesPerSecond: number): string {
  return bytesPerSecond >= 1e6 ? `${(bytesPerSecond / 1e6).toFixed(1)} MB/s` : `${Math.round(bytesPerSecond / 1e3)} KB/s`
}

/** A low rate, or a long time left (or taken, with no estimate): the same rule as the daemon's `load::looks_slow`. */
export function looksSlow(load: PortLoad): boolean {
  if (load.bytes_per_second < SLOW_RATE) return true
  const done = load.received_bytes + (load.cached_bytes ?? 0)
  if (load.expected_bytes != null) return (Math.max(0, load.expected_bytes - done) / load.bytes_per_second) * 1000 > LONG_LOAD_MS
  return load.elapsed_ms > LONG_LOAD_MS
}

/** The changing part of the strip, `4.2 of 31 MB · 90 KB/s`, while a page still loads slowly after `SLOW_AFTER_MS` with
 *  bytes arriving, else null. */
export function slowLoadDetail(load: PortLoad | undefined): string | null {
  if (!load || load.open_requests === 0 || load.elapsed_ms < SLOW_AFTER_MS || load.received_bytes === 0 || !looksSlow(load)) return null
  const done = load.received_bytes + (load.cached_bytes ?? 0)
  return `${amount(done, load.expected_bytes)} · ${rate(load.bytes_per_second)}`
}

/** The whole strip as one line, else null. */
export function slowLoadText(load: PortLoad | undefined): string | null {
  const detail = slowLoadDetail(load)
  return detail ? `${SLOW_LOAD_MESSAGE} — ${detail}` : null
}

/** `via kdi-vps` when the forward's connections go through another member. */
export function routeText(forward: Pick<PortForward, 'route_via'> | undefined): string | null {
  const via = forward?.route_via
  return via ? `via ${shortMachineName(via.machine_name) || via.machine_name}` : null
}
