// The popover under a tile (SPEC §3f): what this agent needs, in a sentence, and the one or two things to do about it. Frosted,
// 280 wide, opens from the tile's centre: opacity, scale .94 → 1, 4 px up, a blur clearing, 200 ms; leaves in 130 ms.
import { useGpuixRequired, type PublicInstance } from '@gpuix/react'
import { useEffect, useState } from 'react'
import { Button } from '../kit/Button.tsx'
import { GlassEdge } from '../kit/GlassEdge.tsx'
import { EXIT, Presence } from '../motion/Presence.tsx'
import { EASE } from '../motion/tokens.ts'
import { addWindowKeyHandler } from '../nav/keys.ts'
import { PATCHED_NATIVE } from '../native/local-build.ts'
import { frostBackdrop, frostFill, useGlass } from '../platform/glass.ts'
import { Label, ProviderMark, REDUCED_MOTION, useTheme } from '../ui.tsx'
import type { ProviderKind } from '../../../../packages/kybern-client/src/types.ts'
import { hasSignIn } from '../../../../packages/kybern-client/src/machineAgents.ts'
import { requestAntigravitySignIn } from '../antigravity/notice.ts'
import { agents, assumeReady, cursorSetup, openPopover, openSheet, recheck, setDefaultAgent, signInWithBrowser, type Agent } from './agentsStore.ts'
import { POPOVER_DEFAULT } from './model.ts'
import { bez } from './motion.tsx'

export const POPOVER_W = 280
/** When, and for which agent, the last press outside closed a popover: the click that follows on its own tile must not reopen it. */
let closed = { at: 0, kind: null as ProviderKind | null }
export const justClosed = (kind: ProviderKind) => closed.kind === kind && Date.now() - closed.at < 250
export const closePopover = () => { closed = { at: Date.now(), kind: agents.get().popover }; openPopover(null) }

function copy(agent: Agent, isDefault: boolean): { lead: string; sentence: string } {
  const { name } = agent
  switch (agent.state) {
    case 'ready': return isDefault
      ? { lead: name, sentence: POPOVER_DEFAULT }
      : { lead: `${name} is ready`, sentence: 'Kybern uses it side by side with your other agents.' }
    case 'signin': return { lead: `Sign in to ${name}`, sentence: 'Kybern uses it as soon as you are signed in.' }
    case 'unknown': return { lead: `${name} is installed`, sentence: 'Kybern can’t tell if you are signed in. Sign in, or check again.' }
    case 'models': return { lead: `${name} lists no models yet`, sentence: `${name} runs on the providers you set up in it. Add one there, then check again. Kybern lists every model it reports.` }
    case 'missing': return { lead: `${name} isn’t installed`, sentence: agent.kind === 'cursor' ? 'Kybern installs Cursor’s SDK for you in one step.' : hasSignIn(agent.kind) ? `Install it in a terminal, then sign in. Nothing runs until you press Return.` : `Install it in a terminal, then set up a provider in it. Nothing runs until you press Return.` }
    case 'update': return { lead: `${name} needs an update`, sentence: agent.reason ? agent.reason.replace(/\s+/g, ' ').slice(0, 160) : `Update ${name}, then check again.` }
    case 'waiting': return { lead: `Signing in to ${name}`, sentence: 'Finish signing in in your browser. This page updates when you are done.' }
    case 'installing': return { lead: `Installing ${name}`, sentence: 'Kybern checks again when it is done.' }
    default: return { lead: name, sentence: '' }
  }
}

function Actions({ agent, isDefault }: { agent: Agent; isDefault: boolean }) {
  const { kind, state } = agent
  const cli = kind !== 'cursor'
  const row = (children: React.ReactNode) => <div style={{ display: 'flex', flexDirection: 'column', gap: 6, alignSelf: 'stretch' }}>{children}</div>
  if (state === 'ready') return isDefault ? null : row(<Button testId="agent-make-default" block onClick={() => { setDefaultAgent(kind); closePopover() }}>Start new chats with it</Button>)
  if (state === 'signin' || state === 'unknown') {
    // Antigravity has no browser flow of its own: `agy` signs in from a terminal, after the one-time notice.
    if (kind === 'antigravity') return row(<>
      <Button testId="agent-signin-terminal" block variant="secondary" onClick={() => requestAntigravitySignIn(() => void openSheet(kind, 'login'))}>Sign in in the terminal</Button>
      {state === 'unknown' && <Button testId="agent-works" block variant="ghost" onClick={() => assumeReady(kind)}>It already works</Button>}
    </>)
    return row(<>
      <Button testId="agent-signin-browser" block variant="secondary" onClick={() => signInWithBrowser(kind)}>Sign in with your browser</Button>
      {cli && kind !== 'opencode' && <Button testId="agent-signin-terminal" block variant="ghost" onClick={() => void openSheet(kind, 'login')}>Sign in in the terminal</Button>}
      {cli && kind === 'opencode' && <Button testId="agent-signin-terminal" block variant="ghost" onClick={() => void openSheet(kind, 'login')}>Sign in in the terminal</Button>}
      {state === 'unknown' && <Button testId="agent-works" block variant="ghost" onClick={() => assumeReady(kind)}>It already works</Button>}
    </>)
  }
  if (state === 'models') return row(<Button testId="agent-works" block variant="ghost" onClick={() => assumeReady(kind)}>It already works</Button>)
  if (state === 'missing') {
    return cli
      ? row(<Button testId="agent-install" block onClick={() => void openSheet(kind, 'install')}>Install in terminal</Button>)
      : row(<Button testId="agent-install" block onClick={() => void cursorSetup('install')}>Install</Button>)
  }
  if (state === 'update') return cli ? row(<Button testId="agent-update" block onClick={() => void openSheet(kind, 'update')}>Update in terminal</Button>) : row(<Button block onClick={() => { closePopover(); void recheck(kind) }}>Check again</Button>)
  if (state === 'waiting') return cli ? row(<Button block variant="secondary" onClick={() => signInWithBrowser(kind)}>Open sign-in again</Button>) : row(<Button block variant="ghost" onClick={() => void cursorSetup('cancel_sign_in')}>Cancel</Button>)
  return null
}

export function AgentPopover({ agent, isDefault, x, y, bounds }: { agent: Agent | null; isDefault: boolean; x: number; y: number; bounds: { min: number; max: number } }) {
  const t = useTheme()
  const g = useGlass()
  const renderer = useGpuixRequired()
  const [node, setNode] = useState<PublicInstance | null>(null)
  const kind = agent?.kind ?? null
  useEffect(() => {
    if (!agent) return
    return addWindowKeyHandler((e) => { if (e.key === 'escape') { closePopover(); return true } return false })
  }, [agent])
  // Focus moves into the popover when it opens and back to what opened it (the tile) when it closes, so a keyboard user does not
  // lose their place and a screen reader reads the popover's name first.
  useEffect(() => {
    if (!kind) return
    const opener = renderer.getFocusedElementId?.()
    return () => {
      // Unconditional: a dialog the popover opens (sign-in, the terminal sheet) takes focus after this, so it still wins.
      if (opener != null) queueMicrotask(() => renderer.focusElement?.(opener))
    }
  }, [kind, renderer])
  useEffect(() => { if (node) renderer.focusElement?.(node.id) }, [node, renderer])
  const left = Math.min(Math.max(x - POPOVER_W / 2, bounds.min), bounds.max - POPOVER_W)
  const origin: [number, number] = [Math.min(1, Math.max(0, (x - left) / POPOVER_W)), 0]
  const enter = PATCHED_NATIVE
    ? REDUCED_MOTION
      ? { initial: { opacity: 0 }, animate: { opacity: 1 }, transition: { duration: 0.15, ease: bez(EASE.out) } }
      : { initial: { opacity: 0, scale: 0.94, translateY: 4, blur: 4 }, animate: { opacity: 1, scale: 1, translateY: 0, blur: 0 }, transition: { duration: 0.2, ease: bez(EASE.out) }, origin }
    : undefined
  const body = agent ? copy(agent, isDefault) : null
  return (
    <Presence show={!!agent} enter={enter} exit={{ ...EXIT.menu, to: REDUCED_MOTION ? { opacity: 0 } : { opacity: 0, scale: 0.96, translateY: 2 }, duration: 0.13 }} style={{ position: 'absolute', left, top: y, width: POPOVER_W }}>
      {agent && body && (
        <div
          ref={setNode}
          testId="agent-popover"
          role="dialog"
          aria-label={body.lead}
          tabIndex={-1}
          onMouseDownOutside={closePopover}
          backdrop={frostBackdrop(g, 'menus', 18)}
          style={{
            position: 'relative', display: 'flex', flexDirection: 'column', gap: 10, padding: 16, borderRadius: 18, borderWidth: 1, borderColor: g.on ? g.frostRim : t.border,
            backgroundColor: frostFill(g, 'menus', t.popover, 'frostMenu'),
            boxShadow: g.on ? undefined : { offsetX: 0, offsetY: 18, blurRadius: 40, spreadRadius: -12, color: t.variant === 'dark' ? 'rgba(0,0,0,0.6)' : 'rgba(13,13,13,0.28)' },
          }}
        >
          <GlassEdge radius={18} color={g.frostHighlight} />
          <div style={{ display: 'flex', flexDirection: 'row', alignItems: 'center', gap: 8 }}>
            <ProviderMark kind={agent.kind} size={18} />
            <Label color={t.text} size={14} lineHeight={19} weight={600} style={{ whiteSpace: 'nowrap' }}>{body.lead}</Label>
          </div>
          {body.sentence && <Label color={t.textSecondary} size={13} lineHeight={19.5} style={{ width: POPOVER_W - 34 }}>{body.sentence}</Label>}
          {agent.state === 'ready' && agent.email && <Label color={t.textTertiary} size={12} lineHeight={16} truncate style={{ width: POPOVER_W - 34 }}>{`Signed in as ${agent.email}`}</Label>}
          <Actions agent={agent} isDefault={isDefault} />
          {agent.state !== 'installing' && agent.state !== 'checking' && (
            <div style={{ display: 'flex', flexDirection: 'row', justifyContent: 'flex-start' }}>
              <Button testId="agent-recheck" size="sm" variant="ghost" onClick={() => { closePopover(); void recheck(agent.kind) }}>Check again</Button>
            </div>
          )}
        </div>
      )}
    </Presence>
  )
}
