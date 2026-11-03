// Words for a machine's state, shared by the sidebar filter, the footer and Settings → Machines. Pure.
import { relativeTime } from '../shared/lib/format.ts'

export interface StatusInput {
  status: 'online' | 'relaying' | 'offline' | 'paused' | 'needs-update' | 'unpaired'
  lastSeenAt?: number
  /** Name of the member a relaying machine goes through. */
  via?: string
  error?: string
}

/** "Offline · last seen 2h ago" reads better than a bare "2h": the relative phrase for a last-seen time. */
export function lastSeen(ms: number | undefined, now = Date.now()): string {
  if (ms === undefined) return 'last seen unknown'
  const ago = relativeTime(new Date(ms).toISOString(), now)
  return ago === 'now' ? 'last seen just now' : /^\d+[mhdw]$/.test(ago) ? `last seen ${ago} ago` : `last seen ${ago}`
}

export function statusText(machine: StatusInput, now = Date.now()): string {
  switch (machine.status) {
    case 'online': return 'Online'
    case 'needs-update': return 'Online · needs an update'
    case 'relaying': return machine.via ? `Relaying via ${machine.via}` : 'Relaying'
    case 'paused': return 'Paused'
    case 'unpaired': return machine.error ?? 'Not paired with this device'
    case 'offline': return `Offline · ${lastSeen(machine.lastSeenAt, now)}`
  }
}

/**
 * The state a machine row shows: what this app's own connection says, except that a machine the daemon the user is at
 * reports online is "Online" even while this app's connection to it is still coming back (it is dialed again at once,
 * ADE-121). Paused and unpaired keep their own words.
 */
export function shownStatus(connection: StatusInput['status'], daemon?: string | null): StatusInput['status'] {
  return connection === 'offline' && daemon === 'online' ? 'online' : connection
}

/** The footer line: "4 machines · 1 offline · 1 paused", and "· 2 waiting" while the outbox holds work for machines that are away. A paused machine is on purpose, so it is not "offline". */
export function footerText(machines: readonly { offline: boolean; paused?: boolean }[], waiting = 0): string {
  const paused = machines.filter((machine) => machine.paused).length
  const offline = machines.filter((machine) => machine.offline && !machine.paused).length
  const parts = [`${machines.length} machines`]
  if (offline) parts.push(`${offline} offline`)
  if (paused) parts.push(`${paused} paused`)
  if (waiting) parts.push(`${waiting} waiting`)
  return parts.join(' · ')
}

/** Dot colours by state: online green, relaying and update amber, the rest grey. */
export type Tone = 'good' | 'warn' | 'idle' | 'bad'
export function statusTone(status: StatusInput['status']): Tone {
  switch (status) {
    case 'online': return 'good'
    case 'relaying': case 'needs-update': return 'warn'
    case 'unpaired': return 'bad'
    default: return 'idle'
  }
}
