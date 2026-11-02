// What a machine card says (Settings → Machines, design §5.2), from the machine set's row, the member card the network
// reported and the machine's own `machines.info`. Pure, so the words are tested without a window.
import { looksGeneratedName, shortMachineName } from '../../../../packages/kybern-client/src/forkMachine.ts'
import { harnessChips, machineSpecs, osKind, type HarnessChip, type MachineSpecs, type OsKind } from '../../../../packages/kybern-client/src/machineSpecs.ts'
import type { KeepReachableStatus, MachineCard, MachineId, MachineInfo } from '../../../../packages/kybern-client/src/types.ts'
import type { MachineRow } from '../machines/index.ts'
import { statusText, statusTone, type Tone } from '../machines/format.ts'

export interface MachineCardModel {
  machineId: string
  /** As people say it: no `.local` or tailnet suffix. */
  name: string
  row: MachineRow
  member: boolean
  tone: Tone
  status: string
  /** The machine's version, shown beside its status. */
  version: string | null
  os: OsKind | null
  /** "macOS", or the raw name of an OS we have no mark for. */
  osLabel: string | null
  /** The Tailscale name: a tooltip on the machine's name. */
  tsName: string | null
  /** CPU, memory and GPUs once the machine answered `machines.info`; empty while unknown. */
  specs: MachineSpecs
  /** Only the harnesses the machine has. */
  harnesses: HarnessChip[]
  /** The machine (or its network card) said which harnesses it has; false for one that never answered. */
  harnessesKnown: boolean
  /** The machine runs an older version than the one the user is at. */
  behind: boolean
  canPause: boolean
  canRename: boolean
}

const OS_NAME: Record<string, string> = { macos: 'macOS', darwin: 'macOS', windows: 'Windows', linux: 'Linux' }
export const osName = (os: string | undefined | null) => (os ? OS_NAME[os.toLowerCase()] ?? os : null)

/** Version numbers compared by their numeric parts; a missing or odd one is never "behind". */
export function isBehind(version: string | undefined | null, latest: string | undefined | null): boolean {
  const parse = (v: string | undefined | null) => (v ?? '').split('-')[0]!.split('.').map((n) => Number.parseInt(n, 10))
  const a = parse(version), b = parse(latest)
  if (!a.length || !b.length || a.some(Number.isNaN) || b.some(Number.isNaN)) return false
  for (let i = 0; i < Math.max(a.length, b.length); i++) if ((a[i] ?? 0) !== (b[i] ?? 0)) return (a[i] ?? 0) < (b[i] ?? 0)
  return false
}

export function buildCards(input: {
  rows: readonly MachineRow[]
  members: readonly MachineCard[]
  infos: Record<string, MachineInfo | false>
  /** The version of the machine the user is at: the newest this window can compare with. */
  latest?: string | null
  now?: number
}): MachineCardModel[] {
  const cards = new Map(input.members.map((card) => [card.machine_id, card]))
  const names = new Map(input.rows.map((row) => [row.machineId, row.name]))
  return input.rows.map((row) => {
    const card = cards.get(row.machineId)
    const version = card?.version ?? row.version
    const harnesses = card?.harnesses?.length ? card.harnesses : []
    const info = input.infos[row.machineId]
    const via = card?.relay_via ? names.get(card.relay_via) : row.relayVia ? names.get(row.relayVia) : undefined
    return {
      machineId: row.machineId,
      name: shortMachineName(row.name) || row.name,
      row,
      member: row.member,
      tone: statusTone(row.status),
      status: statusText({ status: row.status, ...(row.lastSeenAt !== undefined ? { lastSeenAt: row.lastSeenAt } : {}), ...(via ? { via } : {}), ...(row.error ? { error: row.error } : {}) }, input.now),
      version: version || null,
      os: osKind(card?.os ?? row.os),
      osLabel: osName(card?.os ?? row.os),
      tsName: card?.ts_name || null,
      specs: machineSpecs(info),
      harnesses: harnessChips(info && info.harnesses?.length ? info.harnesses : harnesses),
      harnessesKnown: !!(info && info.harnesses) || !!card?.harnesses,
      behind: !row.isLocal && isBehind(version, input.latest),
      canPause: row.member && !row.isLocal,
      canRename: row.member,
    }
  })
}

export { doctorRows, doctorSummary, outboxRows, updateResultRows, updateSummary } from '../../../../packages/kybern-client/src/networkSettings.ts'
export type { DoctorRow, OutboxRowModel, UpdateResultRow } from '../../../../packages/kybern-client/src/networkSettings.ts'

// ---- "Name your machines" ----

/** Members whose name looks made up by a host or an installer (`srv1544252`, `VM-3-60-ubuntu`): the ones worth renaming. */
export function unnamedMachines(cards: readonly Pick<MachineCardModel, 'name' | 'member' | 'canRename'>[]): typeof cards {
  return cards.filter((card) => card.member && card.canRename && looksGeneratedName(card.name))
}

/**
 * "Keep this machine reachable when Kybern is closed": what the switch's row says, or null where this machine cannot
 * register a login item (an older daemon, an unsupported system). A problem replaces the explanation.
 */
export function reachableRow(status: KeepReachableStatus | null | undefined): { description: string; problem: boolean; locked: boolean } | null {
  if (!status?.kind) return null
  if (status.external) return { description: 'Another service on this computer already starts Kybern when you sign in.', problem: false, locked: true }
  const problem = status.error ?? status.warnings?.[0]
  if (problem) return { description: problem, problem: true, locked: false }
  if (!status.enabled) return { description: 'Your other machines reach this one only while Kybern is open.', problem: false, locked: false }
  return { description: 'Kybern starts in the background when you sign in, so your other machines reach this one with the app closed.', problem: false, locked: false }
}
