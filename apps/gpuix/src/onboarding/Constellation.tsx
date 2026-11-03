// The agents constellation (SPEC §3a–c): the Kybern mark in a hub and six agent tiles in two wings around it, two orbit rings, and
// dots that light from a tile in toward the hub when that agent is ready. Calm on purpose: tiles settle to their spots, a ready
// agent swells once, nothing bobs, ripples or sparks. Every tile is absolutely placed, so detection changes cost no layout.
import { useEffect, useRef, useState } from 'react'
import type { ProviderKind } from '../../../../packages/kybern-client/src/types.ts'
import { PaintTranslate } from '../drag/PaintTranslate.tsx'
import { MatrixLoader } from '../MatrixLoader.tsx'
import { Swap } from '../motion/kit.tsx'
import { pressTransform } from '../motion/press.ts'
import { EASE } from '../motion/tokens.ts'
import { PATCHED_NATIVE } from '../native/local-build.ts'
import { Label, LOGO_SVG, ProviderMark, REDUCED_MOTION, useTheme } from '../ui.tsx'
import type { Theme } from '../theme.ts'
import { useLoopRunning } from '../native/loop-visibility.ts'
import { justClosed } from './AgentPopover.tsx'
import { agents, type Agent } from './agentsStore.ts'
import { bez, riseTransform, Roll, SETTLE } from './motion.tsx'
import { Later } from './sceneKit.tsx'
import { STATE_WORD, type AgentState } from './model.ts'

/** Clockwise from the top left: how the tiles arrive. */
const ARRIVAL: readonly ProviderKind[] = ['claude-code', 'cursor', 'omp', 'pi', 'opencode', 'codex', 'antigravity']
const SQUARE = 0.72

export interface Orbit { width: number; height: number; rx: number; ry: number; cx: number; cy: number }
export function orbitFor(width: number, height: number): Orbit {
  return { width, height, rx: Math.min(width / 2 - 76, 300), ry: Math.min(height * 0.34, 118), cx: width / 2, cy: height * 0.44 }
}
/** The seven spots: three on each wing and Antigravity centred on the bottom row, relative to the hub (SPEC §3a). */
export function spotFor(kind: ProviderKind, o: Orbit): { x: number; y: number } {
  const { rx, ry } = o
  // Antigravity sits between OpenCode and pi, a little lower so it clears the hub's "ready" count. Lower than the row's
  // labels and its own label runs into the headline under the scene.
  const at: Record<ProviderKind, [number, number]> = {
    'claude-code': [-SQUARE * rx, -ry], codex: [-rx, 0], opencode: [-SQUARE * rx, ry], cursor: [SQUARE * rx, -ry], omp: [rx, 0], pi: [SQUARE * rx, ry], antigravity: [0, ry + 8],
  }
  return { x: o.cx + at[kind][0], y: o.cy + at[kind][1] }
}

const shadow = (t: Theme, lift = false) => ({ offsetX: 0, offsetY: lift ? 10 : 4, blurRadius: lift ? 26 : 14, spreadRadius: lift ? -8 : -6, color: t.variant === 'dark' ? `rgba(0, 0, 0, ${lift ? 0.55 : 0.4})` : `rgba(13, 13, 13, ${lift ? 0.22 : 0.12})` })

function dotColor(t: Theme, state: AgentState) {
  if (state === 'ready') return t.green
  if (state === 'signin' || state === 'unknown' || state === 'models' || state === 'waiting' || state === 'installing') return t.accentBlue
  if (state === 'update') return t.red
  return t.textTertiary
}
const wordColor = (t: Theme, state: AgentState) => (state === 'signin' || state === 'unknown' || state === 'models' ? t.accentBlue : state === 'update' ? t.red : t.textSecondary)

/** A ring that breathes while the user is mid-task on a tile (sign-in, install): opacity 1 ↔ .35 over 1.2 s, one retarget per half. */
function BreathingRing({ color, running }: { color: string; running: boolean }) {
  const [low, setLow] = useState(false)
  const gen = useRef(1)
  if (!PATCHED_NATIVE || REDUCED_MOTION || !running) return <div style={{ position: 'absolute', left: -5, top: -5, right: -5, bottom: -5, borderRadius: 27, borderWidth: 1.5, borderColor: color, pointerEvents: 'none' }} />
  return (
    <div
      paintTransform={{ initial: false, animate: { opacity: low ? 0.35 : 1 }, transition: { duration: 0.6, ease: bez([0.37, 0, 0.63, 1]) }, generation: gen.current, retain: true }}
      onMotionComplete={(e) => { if (e.motionGeneration === gen.current) { gen.current++; setLow((v) => !v) } }}
      style={{ position: 'absolute', left: -5, top: -5, right: -5, bottom: -5, borderRadius: 27, borderWidth: 1.5, borderColor: color, pointerEvents: 'none' }}
    />
  )
}

function Tile({ agent, x, y, hub, quick, open, onClick, loopOn }: {
  agent: Agent; x: number; y: number; hub: { x: number; y: number }; quick: boolean; open: boolean; onClick: () => void; loopOn: boolean
}) {
  const t = useTheme()
  const [hover, setHover] = useState(false)
  const { state } = agent
  const checking = state === 'checking'
  const working = state === 'waiting' || state === 'installing'
  const dx = x - hub.x, dy = y - hub.y
  // The swell when an agent turns ready after the first reveal; the shake when it needs an update.
  const first = useRef(true)
  const before = useRef<AgentState>(state)
  const [pulse, setPulse] = useState(0)
  useEffect(() => {
    if (first.current) { first.current = false; before.current = state; return }
    if (state === 'ready' && before.current !== 'ready' && before.current !== 'checking') setPulse((n) => n + 1)
    if (state === 'update' && before.current !== 'update') setPulse((n) => n + 1)
    before.current = state
  }, [state])
  const arrive = quick ? { base: 0.06, step: 0.025 } : { base: 0.52, step: 0.06 }
  const delay = arrive.base + ARRIVAL.indexOf(agent.kind) * arrive.step
  const entrance = PATCHED_NATIVE
    ? REDUCED_MOTION
      ? { initial: { opacity: 0 }, animate: { opacity: 1 }, transition: { duration: 0.18, ease: bez(EASE.out) } }
      : { initial: { opacity: 0, translateX: -0.14 * dx, translateY: -0.14 * dy, scale: 0.94 }, animate: { opacity: 1, translateX: 0, translateY: 0, scale: 1 }, transition: { duration: 0.9, delay, ease: bez(EASE.out) } }
    : undefined
  const swell = pulse > 0 && PATCHED_NATIVE && !REDUCED_MOTION
    ? state === 'update'
      ? { initial: { translateX: 0 }, keyframes: [{ offset: 0.16, translateX: -6 }, { offset: 0.4, translateX: 5 }, { offset: 0.65, translateX: -3 }, { offset: 0.85, translateX: 2 }], animate: { translateX: 0 }, transition: { duration: 0.38, ease: bez(EASE.inOut) }, generation: pulse }
      : { initial: { scale: 1 }, keyframes: [{ offset: 0.5, scale: 1.06 }], animate: { scale: 1 }, transition: { duration: 0.56, ease: bez(SETTLE) }, generation: pulse }
    : undefined
  const logoOpacity = checking ? 0.55 : state === 'missing' ? 0.4 : 1
  const ringColor = state === 'ready' ? t.green : state === 'update' ? t.red : t.accentBlue
  return (
    <div paintTransform={entrance} style={{ position: 'absolute', left: x - 66, top: y - 36, width: 132, display: 'flex', flexDirection: 'column', alignItems: 'center' }}>
      <div
        paintTransform={PATCHED_NATIVE && !REDUCED_MOTION ? { initial: false, animate: { translateY: hover || open ? -3 : 0 }, transition: { duration: 0.3, ease: bez(EASE.out) }, retain: true } : undefined}
        style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 0 }}
      >
        <div paintTransform={swell as never} style={{ display: 'flex' }}>
          <div
            testId={`agent-${agent.kind}`}
            role="button"
            tabIndex={0}
            aria-label={`${agent.name}, ${STATE_WORD[state]}`}
            onClick={checking ? undefined : onClick}
            onKeyDown={(e) => { if (!checking && (e.key === 'enter' || e.key === 'space')) onClick() }}
            onMouseEnter={() => setHover(true)}
            onMouseLeave={() => setHover(false)}
            paintTransform={checking ? undefined : pressTransform(true, 0.97)}
            style={{
              position: 'relative', display: 'flex', alignItems: 'center', justifyContent: 'center', width: 72, height: 72, borderRadius: 22, backgroundColor: t.surface,
              borderWidth: 1, borderColor: t.cardEdge, boxShadow: shadow(t, hover || open), cursor: checking ? 'default' : 'pointer',
              focusVisible: { outlineWidth: 2, outlineColor: t.accentBlue, outlineOffset: 3 },
            }}
          >
            <div paintTransform={PATCHED_NATIVE ? { initial: false, animate: { opacity: logoOpacity }, transition: { duration: 0.4, ease: bez(EASE.out) }, retain: true } : undefined} style={{ display: 'flex', pointerEvents: 'none' }}>
              <ProviderMark kind={agent.kind} size={32} />
            </div>
            {checking && (
              <div style={{ position: 'absolute', right: 6, bottom: 6, width: 20, height: 20, borderRadius: 10, backgroundColor: t.surface, display: 'flex', alignItems: 'center', justifyContent: 'center', pointerEvents: 'none' }}>
                {loopOn && <MatrixLoader />}
              </div>
            )}
            {(state === 'ready' || working || state === 'update') && (
              state === 'ready'
                ? <div paintTransform={PATCHED_NATIVE ? { initial: { opacity: 0 }, animate: { opacity: 1 }, transition: { duration: 0.5, ease: bez(EASE.out) } } : undefined} style={{ position: 'absolute', left: -5, top: -5, right: -5, bottom: -5, borderRadius: 27, borderWidth: 1.5, borderColor: t.green, pointerEvents: 'none' }} />
                : <BreathingRing color={ringColor} running={working && loopOn} />
            )}
            {state === 'installing' && (
              <div style={{ position: 'absolute', left: 10, right: 10, bottom: 5, height: 3, borderRadius: 2, backgroundColor: t.variant === 'dark' ? 'rgba(255,255,255,0.12)' : 'rgba(13,13,13,0.08)', overflow: 'hidden', pointerEvents: 'none' }}>
                <div paintTransform={PATCHED_NATIVE ? { initial: { scaleX: 0.05 }, animate: { scaleX: 0.92 }, transition: { duration: 25, ease: bez(EASE.out) }, origin: [0, 0.5] } : undefined} style={{ width: '100%', height: 3, borderRadius: 2, backgroundColor: t.accentBlue }} />
              </div>
            )}
          </div>
        </div>
        <div style={{ height: 9 }} />
        <Label color={t.text} size={13} lineHeight={17} weight={600} style={{ whiteSpace: 'nowrap', pointerEvents: 'none' }}>{agent.name}</Label>
        <div style={{ height: 3 }} />
        <div style={{ display: 'flex', flexDirection: 'row', alignItems: 'center', gap: 6, pointerEvents: 'none' }}>
          <div style={{ width: 6, height: 6, borderRadius: 3, backgroundColor: dotColor(t, state), flexShrink: 0 }} />
          <Swap active={state} blur={false} style={{ justifyContent: 'flex-start' }}>
            <Label color={wordColor(t, state)} size={12} lineHeight={16} style={{ whiteSpace: 'nowrap' }}>{STATE_WORD[state]}</Label>
          </Swap>
        </div>
      </div>
    </div>
  )
}

/** Five 3 px dots from a tile in toward the hub; they light one after another when that agent is ready (SPEC §3c). */
function Dots({ x, y, hub, ready }: { x: number; y: number; hub: { x: number; y: number }; ready: boolean }) {
  const t = useTheme()
  const dx = hub.x - x, dy = hub.y - y
  const length = Math.hypot(dx, dy)
  const start = 50, end = length - 74
  // No room between the tile and the hub (Antigravity, right under it): the dots would land on the hub's "ready" count,
  // so the tile's ring alone says it is ready.
  if (end < start + 20) return null
  return (
    <>
      {[0, 1, 2, 3, 4].map((i) => {
        const d = start + ((end - start) * i) / 4
        return (
          <div
            key={i}
            paintTransform={PATCHED_NATIVE ? { initial: false, animate: { opacity: ready ? 1 : 0 }, transition: { duration: ready ? 0.4 : 0.2, delay: ready && !REDUCED_MOTION ? 0.15 + i * 0.07 : 0, ease: bez(EASE.out) }, retain: true } : undefined}
            style={{ position: 'absolute', left: x + (dx / length) * d - 1.5, top: y + (dy / length) * d - 1.5, width: 3, height: 3, borderRadius: 1.5, backgroundColor: t.textTertiary, pointerEvents: 'none', ...(PATCHED_NATIVE ? {} : { opacity: ready ? 1 : 0 }) }}
          />
        )
      })}
    </>
  )
}

const ellipse = (cx: number, cy: number, rx: number, ry: number, pad: number) =>
  `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${cx * 2 + pad * 2} ${cy * 2 + pad * 2}"><ellipse cx="${cx + pad}" cy="${cy + pad}" rx="${rx}" ry="${ry}" fill="none" stroke="#000" stroke-width="1" stroke-dasharray="3 6"/></svg>`

export function Constellation({ agents: list, ready, defaultKind, orbit, quick }: { agents: Agent[]; ready: number; defaultKind: ProviderKind | null; orbit: Orbit; quick: boolean }) {
  const t = useTheme()
  const open = agents.use((s) => s.popover)
  const loop = useLoopRunning(true)
  const [connects, setConnects] = useState(0)
  const seen = useRef<number | null>(null)
  useEffect(() => {
    if (seen.current !== null && ready > seen.current) setConnects((n) => n + 1)
    seen.current = ready
  }, [ready])

  const hub = { x: orbit.cx, y: orbit.cy }
  const outerRy = orbit.ry / Math.sqrt(1 - SQUARE * SQUARE)
  const pad = 40
  const ringStyle = { position: 'absolute' as const, left: -pad, top: -pad, width: orbit.width + pad * 2, height: orbit.height + pad * 2, pointerEvents: 'none' as const }
  const ringTint = t.variant === 'dark' ? 'rgba(255, 255, 255, 0.1)' : 'rgba(13, 13, 13, 0.09)'
  const badge = defaultKind ? spotFor(defaultKind, orbit) : null
  return (
    <div ref={loop.ref} testId="agents-orbit" style={{ position: 'relative', width: orbit.width, height: orbit.height, flexShrink: 0 }}>
      {/* The rings each rasterize an orbit-wide SVG, the tiles six marks and their labels: each mounts a frame apart (`Later`). */}
      <Later frames={4}>
      <div paintTransform={riseTransformRing(0.3, 1.2)} style={ringStyle}>
        <svg source={ellipse(orbit.cx, orbit.cy, orbit.rx, outerRy, pad)} style={{ width: orbit.width + pad * 2, height: orbit.height + pad * 2, color: ringTint, pointerEvents: 'none' }} />
      </div>
      </Later>
      <Later frames={3}>
      <div paintTransform={riseTransformRing(0.2, 1.2)} style={ringStyle}>
        <svg source={ellipse(orbit.cx, orbit.cy, orbit.rx * 0.56, outerRy * 0.56, pad)} style={{ width: orbit.width + pad * 2, height: orbit.height + pad * 2, color: ringTint, pointerEvents: 'none' }} />
      </div>
      </Later>
      {list.map((agent) => {
        const spot = spotFor(agent.kind, orbit)
        return <Dots key={agent.kind} x={spot.x} y={spot.y} hub={hub} ready={agent.state === 'ready'} />
      })}
      <div
        paintTransform={PATCHED_NATIVE ? { initial: REDUCED_MOTION ? { opacity: 0 } : { opacity: 0, scale: 0.9 }, animate: { opacity: 1, scale: 1 }, transition: { duration: 0.7, delay: 0.1, ease: bez(EASE.out) } } : undefined}
        style={{ position: 'absolute', left: hub.x - 56, top: hub.y - 56, width: 112, height: 112, borderRadius: 56, backgroundColor: t.surface, borderWidth: 1, borderColor: t.cardEdge, boxShadow: shadow(t, false), display: 'flex', alignItems: 'center', justifyContent: 'center' }}
      >
        <div style={{ width: 52, height: 33, display: 'flex' }}>
          <div
            paintTransform={connects > 0 && PATCHED_NATIVE && !REDUCED_MOTION ? { initial: { scale: 1 }, keyframes: [{ offset: 0.5, scale: 1.06 }], animate: { scale: 1 }, transition: { duration: 0.56, delay: 0.52, ease: bez(SETTLE) }, generation: connects } : undefined}
            style={{ width: 52, height: 33 }}
          >
            <svg source={LOGO_SVG} style={{ width: 52, height: 33, color: t.text, pointerEvents: 'none' }} />
          </div>
        </div>
      </div>
      <div style={{ position: 'absolute', left: hub.x - 60, top: hub.y + 62, width: 120, display: 'flex', flexDirection: 'row', justifyContent: 'center', alignItems: 'center', gap: 4, pointerEvents: 'none' }}>
        <ReadyCount ready={ready} color={t.textSecondary} />
      </div>
      <Later frames={2}>
      {list.map((agent) => {
        const spot = spotFor(agent.kind, orbit)
        return <Tile key={agent.kind} agent={agent} x={spot.x} y={spot.y} hub={hub} quick={quick} open={open === agent.kind} loopOn={loop.running} onClick={() => { if (justClosed(agent.kind)) return; agents.set({ popover: open === agent.kind ? null : agent.kind }) }} />
      })}
      {badge && <DefaultBadge x={badge.x + 22} y={badge.y - 46} />}
      </Later>
    </div>
  )
}

const riseTransformRing = (delay: number, duration: number) => PATCHED_NATIVE
  ? REDUCED_MOTION
    ? { initial: { opacity: 0 }, animate: { opacity: 1 }, transition: { duration: 0.18, ease: bez(EASE.out) } }
    : { initial: { opacity: 0, scale: 0.82 }, animate: { opacity: 1, scale: 1 }, transition: { duration, delay, ease: bez(EASE.out) } }
  : undefined

function ReadyCount({ ready, color }: { ready: number; color: string }) {
  // The number rolls; "ready" stays put.
  return (
    <>
      <RollInline value={ready} color={color} />
      <Label color={color} size={12} lineHeight={16} weight={500}>ready</Label>
    </>
  )
}
const RollInline = ({ value, color }: { value: number; color: string }) => <Roll value={value} size={12} lineHeight={16} color={color} weight={500} />

/** One DEFAULT badge that moves between tiles instead of one per tile (SPEC §3c). */
function DefaultBadge({ x, y }: { x: number; y: number }) {
  const t = useTheme()
  const first = useRef(true)
  const appear = first.current
  first.current = false
  return (
    <PaintTranslate
      initial={false}
      animate={{ translateX: x, translateY: y }}
      transition={REDUCED_MOTION ? { duration: 0 } : { type: 'spring', duration: 0.6, bounce: 0 }}
      style={{ position: 'absolute', left: 0, top: 0, pointerEvents: 'none' }}
    >
      <div paintTransform={appear ? riseTransform({ duration: 0.42, y: 4, scale: 0.85 }) : undefined} style={{ display: 'flex', alignItems: 'center', height: 20, paddingLeft: 8, paddingRight: 8, borderRadius: 10, backgroundColor: t.green }}>
        <Label color="#ffffff" size={10.5} lineHeight={14} weight={700} style={{ whiteSpace: 'nowrap' }}>DEFAULT</Label>
      </div>
    </PaintTranslate>
  )
}
