// The terminal sheet (SPEC §3f): rises from the window's bottom edge with the install or sign-in command typed and never run.
// Return runs it. When the program ends, "Close and check" closes the sheet and checks that agent again.
import { Button } from '../kit/Button.tsx'
import { GlassEdge } from '../kit/GlassEdge.tsx'
import { Kbd } from '../kit/Kbd.tsx'
import { Icon } from '../icons.tsx'
import { EASE } from '../motion/tokens.ts'
import { Presence } from '../motion/Presence.tsx'
import { N } from '../nav/glyphs.ts'
import { useNativeWindowSize } from '../native/layout-hooks.ts'
import { PATCHED_NATIVE } from '../native/local-build.ts'
import { frostBackdrop, frostFill, useGlass } from '../platform/glass.ts'
import { Screen as TerminalScreen } from '../terminal/Terminal.tsx'
import { Label, REDUCED_MOTION, useTheme } from '../ui.tsx'
import type { TerminalInfo } from '../../../../packages/kybern-client/src/types.ts'
import { agents, closeSheet, sheetExited } from './agentsStore.ts'
import { bez } from './motion.tsx'

/** The key that runs the typed command, named the way the keyboard labels it. */
const RUN_KEY = process.platform === 'darwin' ? 'Return' : 'Enter'

const HEIGHT = 330

/** What the sheet shows: its title and the terminal (`openTypedTerminal`), once it exists. */
export interface TerminalSheetState {
  title: string
  info: TerminalInfo | null
  /** The program in the terminal ended. */
  exited: boolean
  error: string | null
}

/** The agents scene's sheet. */
export function TerminalSheet() {
  const sheet = agents.use((s) => s.sheet)
  return <TerminalSheetView sheet={sheet} onClose={closeSheet} onExit={sheetExited} />
}

/** The sheet itself, for any typed-command terminal (`testId` prefixes its parts). */
export function TerminalSheetView({ sheet, onClose, onExit, testId = 'agent-sheet' }: { sheet: TerminalSheetState | null; onClose: () => void; onExit: () => void; testId?: string }) {
  const t = useTheme()
  const g = useGlass()
  // Resize events, not a poll: App.tsx mounts the gh sheet for the window's life, and off macOS every poll is a waited
  // UI-thread call (0.44 per key while typing, #229's smoke).
  const size = useNativeWindowSize()
  const width = Math.min(620, (size?.width ?? 1200) - 40)
  const enter = PATCHED_NATIVE
    ? REDUCED_MOTION
      ? { initial: { opacity: 0 }, animate: { opacity: 1 }, transition: { duration: 0.18, ease: bez(EASE.out) } }
      : { initial: { opacity: 0, translateY: HEIGHT + 40 }, animate: { opacity: 1, translateY: 0 }, transition: { duration: 0.48, ease: bez(EASE.drawer) } }
    : undefined
  const exit = { to: REDUCED_MOTION ? { opacity: 0 } : { opacity: 0, translateY: HEIGHT + 40 }, duration: 0.28, ease: EASE.drawer }
  const done = !!sheet?.exited
  return (
    <Presence show={!!sheet} enter={enter} exit={exit} style={{ position: 'absolute', left: ((size?.width ?? 1200) - width) / 2, bottom: 20, width, height: HEIGHT }}>
      {sheet && (
        <div
          testId={testId}
          backdrop={frostBackdrop(g, 'menus', 20)}
          style={{
            position: 'relative', display: 'flex', flexDirection: 'column', width, height: HEIGHT, borderRadius: 20, borderWidth: 1, borderColor: g.on ? g.frostRim : t.border, overflow: 'hidden',
            backgroundColor: frostFill(g, 'menus', t.popover, 'frostMenu'),
            boxShadow: g.on ? undefined : { offsetX: 0, offsetY: 24, blurRadius: 60, spreadRadius: -16, color: t.variant === 'dark' ? 'rgba(0,0,0,0.7)' : 'rgba(13,13,13,0.34)' },
          }}
        >
          <GlassEdge radius={20} color={g.frostHighlight} />
          <div style={{ display: 'flex', flexDirection: 'row', alignItems: 'center', gap: 8, height: 48, paddingLeft: 18, paddingRight: 10, flexShrink: 0 }}>
            <Icon name="console" size={15} color={t.textSecondary} />
            <Label color={t.text} size={14} lineHeight={19} weight={600} style={{ whiteSpace: 'nowrap' }}>{sheet.title}</Label>
            <Label color={t.textTertiary} size={13} lineHeight={18}>Terminal</Label>
            <div style={{ flexGrow: 1 }} />
            <Button testId={`${testId}-close`} size="sm" variant="ghost" icon={N.x} label="Close terminal" onClick={onClose} />
          </div>
          <div style={{ display: 'flex', flexDirection: 'column', flexGrow: 1, minHeight: 0, marginLeft: 12, marginRight: 12, borderRadius: 12, overflow: 'hidden', backgroundColor: t.surface, borderWidth: 1, borderColor: t.cardEdge }}>
            {sheet.error
              ? <div style={{ padding: 16 }}><Label color={t.red} size={13} lineHeight={19}>{`Couldn’t open a terminal. ${sheet.error}`}</Label></div>
              : sheet.info
                ? <TerminalScreen key={sheet.info.id} info={sheet.info} width={width - 26} onExit={onExit} />
                : <div style={{ flexGrow: 1 }} />}
          </div>
          <div style={{ display: 'flex', flexDirection: 'row', alignItems: 'center', gap: 10, height: 56, paddingLeft: 18, paddingRight: 12, flexShrink: 0 }}>
            {done
              ? <Label color={t.green} size={13} lineHeight={18} weight={500}>Done. Close the terminal and Kybern checks again.</Label>
              : <>
                  <Kbd keys={[RUN_KEY]} />
                  <Label color={t.textSecondary} size={13} lineHeight={18}>{`Press ${RUN_KEY} to run it. Nothing runs until you do.`}</Label>
                </>}
            <div style={{ flexGrow: 1 }} />
            {done
              ? <Button testId={`${testId}-check`} variant="primary" onClick={onClose}>Close and check</Button>
              : <Button testId={`${testId}-done`} variant="ghost" onClick={onClose}>Close</Button>}
          </div>
        </div>
      )}
    </Presence>
  )
}
