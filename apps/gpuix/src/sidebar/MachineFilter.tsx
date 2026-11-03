// "All machines ▾": the sidebar header's filter (design §5.2). It takes the place of the environment row and only
// exists while more than one machine is known. The choice is per window and kept with the other sidebar preferences.
import { useState } from 'react'
import { useMachines, type MachineRow } from '../machines/index.ts'
import { daemonLinkDown, footerText, LINK_DOWN_TEXT, shownStatus, statusText, statusTone, type Tone } from '../machines/format.ts'
import { useNetwork } from '../machines/network.ts'
import { Icon } from '../icons.tsx'
import { N } from '../nav/glyphs.ts'
import { Menu, MenuItem, MenuLabel, MenuSeparator, muted } from '../nav/overlay.tsx'
import { setNav, useNav } from '../nav/store.ts'
import { localProfile } from '../environments/registry.ts'
import { useApp } from '../state.ts'
import { Label, useTheme } from '../ui.tsx'
import { accent } from './rows.tsx'
import { MachineBadge } from './MachineBadge.tsx'

const toneColor = (t: ReturnType<typeof useTheme>, tone: Tone) => (tone === 'good' ? '#00bc7d' : tone === 'warn' ? '#e8a317' : tone === 'bad' ? t.red : muted(t, 0.4))

export function StatusDot({ tone }: { tone: Tone }) {
  const t = useTheme()
  return <div style={{ width: 6, height: 6, borderRadius: 3, backgroundColor: toneColor(t, tone), flexShrink: 0 }} />
}

/** The machine the filter names, or `null` for all (a machine that is gone counts as all). */
export function activeMachineFilter(machines: readonly MachineRow[], filter: string | null): MachineRow | null {
  return filter ? machines.find((machine) => machine.machineId === filter) ?? null : null
}

/**
 * The rows as the picker shows them: a machine the daemon reports online reads "Online" (`shownStatus`), and one this
 * app reaches while the daemon here has no session to it is marked (`linkDown`), so it never reads a plain "Online".
 */
export function pickerRows(machines: readonly MachineRow[], cards: readonly { machine_id: string; status: string }[] | undefined): MachineRow[] {
  if (!cards?.length) return machines as MachineRow[]
  const daemon = new Map(cards.map((card) => [card.machine_id, card.status]))
  return machines.map((machine) => {
    const reported = daemon.get(machine.machineId)
    if (daemonLinkDown(machine.status, reported, machine.isLocal)) return { ...machine, linkDown: true }
    const status = shownStatus(machine.status, reported)
    return status === machine.status ? machine : { ...machine, status, offline: false }
  })
}

/** What a picker row says under the machine's name. */
export function pickerDetail(machine: MachineRow): string {
  if (machine.linkDown) return LINK_DOWN_TEXT
  return statusText({ status: machine.status, ...(machine.lastSeenAt !== undefined ? { lastSeenAt: machine.lastSeenAt } : {}) }).replace(/^Offline · /, '')
}

export function MachineFilter() {
  const t = useTheme()
  const net = useNetwork()
  const machines = pickerRows(useMachines(), net.status?.members)
  const filter = useNav((s) => s.machineFilter)
  const [open, setOpen] = useState(false)
  // One machine: the row looks as the environment row always did (this computer, a status dot, a chevron) and opens Settings → Machines, where the others are added.
  if (machines.length < 2) return <SingleMachine />
  const chosen = activeMachineFilter(machines, filter)
  const offline = machines.filter((machine) => machine.offline && !machine.paused).length
  const tone: Tone = chosen ? (chosen.linkDown ? 'warn' : statusTone(chosen.status)) : offline || machines.some((machine) => machine.linkDown) ? 'warn' : 'good'
  const choose = (id: string | null) => setNav({ machineFilter: id })
  return <div style={{ display: 'flex', paddingLeft: 8, paddingRight: 8, paddingBottom: 6 }}>
    <Menu testId="machine-filter" open={open} onOpenChange={setOpen} width={304} rootStyle={{ flexGrow: 1 }} trigger={() =>
      <div aria-label="Filter threads by machine" style={{ display: 'flex', alignItems: 'center', gap: 8, flexGrow: 1, minWidth: 0, minHeight: 32, paddingLeft: 8, paddingRight: 8, paddingTop: 4, paddingBottom: 4, borderRadius: 10, cursor: 'pointer', hover: { backgroundColor: accent(t) } }}>
        {chosen ? <MachineBadge glyph={chosen.glyph} size={16} /> : <Icon source={N.laptop} size={14} color={t.text} />}
        <Label color={t.text} truncate style={{ flexGrow: 1, pointerEvents: 'none' }}>{chosen ? chosen.name : 'All machines'}</Label>
        <StatusDot tone={tone} /><Icon source={N.chevronDown} size={12} color={muted(t, 1)} />
      </div>}>
      <MenuLabel>Machines</MenuLabel>
      <MenuItem id="all" testId="machine-filter-all" iconNode={<Icon source={N.laptop} size={16} color={t.text} />} label="All machines" checked={!chosen} onSelect={() => choose(null)} />
      <MenuSeparator />
      {machines.map((machine) => <MenuItem key={machine.machineId} id={machine.machineId} testId={`machine-filter-${machine.machineId}`}
        iconNode={<MachineBadge glyph={machine.glyph} size={16} />} label={machine.isLocal ? `${machine.name} (this computer)` : machine.name}
        detail={pickerDetail(machine)}
        checked={chosen?.machineId === machine.machineId} onSelect={() => choose(machine.machineId)} />)}
    </Menu>
  </div>
}

/** The sidebar footer: "4 machines · 1 offline · 1 paused", plus "· 2 waiting" while the outbox holds work. It opens Settings → Machines. */
export function MachinesFooter() {
  const t = useTheme()
  const net = useNetwork()
  const machines = pickerRows(useMachines(), net.status?.members)
  if (machines.length < 2) return null
  const open = () => void import('../settings/SettingsScreen.tsx').then((m) => m.openSettingsTab('machines'))
  return <div testId="machines-footer" role="button" tabIndex={0} aria-label="Open machines settings" onClick={open} onKeyDown={(e) => { if (e.key === 'enter' || e.key === 'space') open() }}
    style={{ display: 'flex', alignItems: 'center', gap: 6, flexShrink: 0, minHeight: 28, marginLeft: 8, marginRight: 8, marginBottom: 6, paddingLeft: 8, paddingRight: 8, borderRadius: 8, cursor: 'pointer', hover: { backgroundColor: accent(t) } }}>
    <StatusDot tone={machines.some((machine) => machine.offline && !machine.paused) ? 'warn' : 'good'} />
    <Label color={muted(t, 0.72)} size={12} lineHeight={16} truncate style={{ pointerEvents: 'none' }}>{footerText(machines, net.status?.outbox.queued ?? 0)}</Label>
  </div>
}

/**
 * The sidebar's lists for the machine filter: threads of the chosen machine, and the projects that have a checkout there
 * (free chat always). With no filter, or a machine that is gone, nothing changes and the same arrays come back.
 */
export function filterByMachine<P extends { id: string }, T extends { machineId?: string }>(projects: P[], threads: T[], machineId: string | null, local: string | undefined, machinesOf: (projectId: string) => readonly string[], isFree: (projectId: string) => boolean): { projects: P[]; threads: T[] } {
  if (!machineId) return { projects, threads }
  const owner = (thread: T) => thread.machineId ?? local
  return {
    projects: projects.filter((project) => isFree(project.id) || (machinesOf(project.id).length ? machinesOf(project.id).includes(machineId) : machineId === local)),
    threads: threads.filter((thread) => owner(thread) === machineId),
  }
}

function SingleMachine() {
  const t = useTheme()
  const status = useApp((s) => s.status)
  const open = () => void import('../settings/SettingsScreen.tsx').then((m) => m.openSettingsTab('machines'))
  return <div style={{ display: 'flex', paddingLeft: 8, paddingRight: 8, paddingBottom: 6 }}>
    <div testId="machine-single" role="button" tabIndex={0} aria-label="Machines" onClick={open} onKeyDown={(e) => { if (e.key === 'enter' || e.key === 'space') open() }} style={{ display: 'flex', alignItems: 'center', gap: 8, flexGrow: 1, minWidth: 0, minHeight: 32, paddingLeft: 8, paddingRight: 8, paddingTop: 4, paddingBottom: 4, borderRadius: 10, cursor: 'pointer', hover: { backgroundColor: accent(t) } }}>
      <Icon source={N.laptop} size={14} color={t.text} /><Label color={t.text} truncate style={{ flexGrow: 1, pointerEvents: 'none' }}>{localProfile.name}</Label>
      <div style={{ pointerEvents: 'none', width: 6, height: 6, borderRadius: 3, backgroundColor: status === 'open' ? '#00bc7d' : status === 'failed' ? t.red : muted(t, 0.4), flexShrink: 0 }} /><Icon source={N.chevronDown} size={12} color={muted(t, 1)} />
    </div>
  </div>
}
