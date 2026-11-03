// "Forward a port from…": pick a machine and the port of a server there; it opens in the Browser through a forward to this machine.
import { useState } from 'react'
import { useMachines } from '../machines/index.ts'
import { Button } from '../kit/Button.tsx'
import { IconButton } from '../kit/IconButton.tsx'
import { TextField } from '../kit/Field.tsx'
import { PickerChip } from '../kit/PickerChip.tsx'
import { Menu, MenuItem } from '../nav/overlay.tsx'
import { N } from '../nav/glyphs.ts'
import { useTheme } from '../ui.tsx'
import { shortMachineName } from '../../../../packages/kybern-client/src/forkMachine.ts'
import { parseForwardTarget } from './remote.ts'
import { openRemoteServer, previewState, setPreview, usePreview } from './store.ts'

/** The machines a server can be forwarded from: every other machine that is reachable now. */
export function useForwardSources() {
  return useMachines().filter((machine) => !machine.isLocal && !machine.offline)
}

export function PortForwardBar({ threadId }: { threadId: string }) {
  const t = useTheme(), s = usePreview(threadId), sources = useForwardSources()
  const [port, setPort] = useState('')
  const chosen = sources.find((machine) => machine.machineId === s.forwardMachine) ?? sources[0]
  const close = () => setPreview(threadId, { forwardOpen: false })
  const submit = () => {
    if (!chosen || previewState(threadId).busy) return
    const target = parseForwardTarget(port)
    if ('error' in target) { setPreview(threadId, { error: target.error }); return }
    void openRemoteServer(threadId, { id: chosen.machineId, name: shortMachineName(chosen.name), port: target.port, path: target.path }, false, 'manual').then((opened) => { if (opened) close() })
  }
  return <div testId="preview-forward-bar" style={{ display: 'flex', alignItems: 'center', height: 36, gap: 6, paddingLeft: 8, paddingRight: 6, flexShrink: 0, borderBottomWidth: 1, borderColor: t.surfaceDivider }}>
    {chosen ? <Menu testId="preview-forward-machine" trigger={(open) => <PickerChip size="sm" open={open}>{shortMachineName(chosen.name)}</PickerChip>}>
      {sources.map((machine) => <MenuItem key={machine.machineId} id={machine.machineId} label={shortMachineName(machine.name)} checked={machine.machineId === chosen.machineId} onSelect={() => setPreview(threadId, { forwardMachine: machine.machineId })} />)}
    </Menu> : <Button size="sm" variant="ghost" disabled>No other machine is online</Button>}
    <TextField testId="preview-forward-port" label="Port" value={port} placeholder="Port, such as 5173" onChange={setPort} onSubmit={submit} shell={{ flexGrow: 1, minWidth: 0 }} />
    <Button size="sm" variant="primary" testId="preview-forward-go" disabled={!chosen || !port.trim() || s.busy} onClick={submit}>Forward</Button>
    <IconButton icon={N.x} label="Close" size="sm" onClick={close} />
  </div>
}
