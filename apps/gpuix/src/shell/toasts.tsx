import { Button } from '../kit/Button.tsx'
import { NOTIFICATION_INSET } from './toastPlacement.ts'
// Toasts (issue #108 §23): the desktop's sonner `Toaster` at the bottom right
// (`components/ui/sonner.tsx`, `views/AppUpdate.tsx`), on GPUIX.
//
//   toast('Saved to “Notes”', { action: { label: 'Open', onClick } })
//   toast.error('Unable to load thread', { description })
//   toast.dismiss(id)
//   toast('Enjoying Kybern?', { description, icon: { source, tone: 'warning' }, cancel: { label: 'Dismiss' }, action, duration: Infinity, onClose })
//
// Geometry follows sonner: 356px wide, 16px padding, 14px between toasts, 16px
// from the window corner (over the composer when they meet), three visible (the newest), the popover surface with
// its 1px border, 13px text, a 16px icon, and an ink action pill (24px, 4px
// radius, 12px/500). A toast with a `cancel` (a quiet second button) is a prompt: its buttons sit on their own row under the
// text, trailing, the primary last, and its icon aligns with the title's first line. A toast times out after 4 s unless `duration` says
// otherwise (`Infinity` stays until dismissed: approvals). The timer stops while
// the pointer is over the stack and while the window is hidden, and resumes with
// the time that was left. No timer exists while no toast does.
//
// Motion: a toast cross-fades in, 250 ms on the desktop's `--toast-ease`: opacity 0 -> 1 while a
// 4px blur clears and it settles from 97% scale (`.t-toast`, `styles/motion.css`, minus its rise:
// toasts do not slide). It leaves in 150 ms (opacity, a lighter blur, 98.5%), so exits are faster
// than entrances. A toast replaced by another, a card that drops back into the collapsed stack and a
// ghost that steps forward all cross-fade the same way instead of popping. All of it is the paint-only
// `paintTransform` on the card (no render per frame), and it retargets if a toast is dismissed
// mid-entrance. Under Reduce Motion a toast only fades, 120 ms. A toast is dismissed with its hover
// close button, not by swiping.
import {
  AlertCircleIcon,
  Alert02Icon,
  Cancel01Icon,
  CheckmarkCircle02Icon,
  InformationCircleIcon,
} from '@hugeicons/core-free-icons'
import { useGpuixRequired, useWindowSize, type PublicInstance } from '@gpuix/react'
import type { PaintTransformProps } from '@gpuix/native/host'
import { useEffect, useRef, useState, useSyncExternalStore } from 'react'
import { glyph, Icon } from '../icons.tsx'
import '../native/backdrop.ts'
import { onWindowVisibility, isWindowVisible } from '../native/visibility.ts'
import { EASE } from '../motion/kit.tsx'
import { PATCHED_NATIVE } from '../native/local-build.ts'
import { Label, REDUCED_MOTION, useTheme } from '../ui.tsx'
import { GlassEdge } from '../kit/GlassEdge.tsx'
import { toneColor, type Tone } from '../kit/tone.ts'
import { frostBackdrop, frostFill, useGlass } from '../platform/glass.ts'

export type ToastKind = 'default' | 'success' | 'info' | 'warning' | 'error'

export interface ToastOptions {
  /** Updates the toast with this id in place instead of stacking another. */
  id?: string
  description?: string
  action?: { label: string; onClick: () => void }
  /** A quiet second button before `action` (the toast becomes a prompt with its buttons under the text). It closes the toast. */
  cancel?: { label: string; onClick?: () => void }
  /** A glyph source for the leading icon, in the tone's colour, instead of the kind's icon. */
  icon?: { source: string; tone?: Tone }
  /** Runs once when the toast closes by any path: its close button, either button, `toast.dismiss`, its timer. */
  onClose?: () => void
  /** 0…1: a progress bar under the text. A toast with this id and title updates in place as it advances (no cross-fade per tick). */
  progress?: number
  /** Milliseconds before it leaves; `Infinity` keeps it until dismissed. Default 4000. */
  duration?: number
}

interface Item {
  id: string
  /** Unique per card: a toast that replaces another keeps its id, so layers and removal go by this. */
  key: string
  kind: ToastKind
  title: string
  description?: string
  action?: { label: string; onClick: () => void }
  cancel?: { label: string; onClick?: () => void }
  icon?: { source: string; tone?: Tone }
  onClose?: () => void
  progress?: number
  duration: number
  /** The card's height in px: measured once painted, estimated before. */
  height: number
  leaving: boolean
  /** Time left on the timer while it is paused or not yet started. */
  remaining: number
  /** When the running timer started, or null while it is not running. */
  startedAt: number | null
  timer?: ReturnType<typeof setTimeout>
}

const WIDTH = 356
const GAP = 14
const VISIBLE = 3
/** How far each older card of a collapsed stack peeks above the one in front. */
const PEEK = 12.5
/** A one-line toast: 16px padding either side, a 20px title, the border. */
const ESTIMATE = 54
const DEFAULT_DURATION = 4000
/** `--toast-open` less the rise it dropped; the curve is `--toast-ease`. */
const ENTER = 0.25
const EXIT = 0.15
const FADE = 0.12

let items: Item[] = []
let snapshot: readonly Item[] = items
let hovered = false
let hidden = false
let nextId = 1
let nextKey = 1
const listeners = new Set<() => void>()

function commit() {
  snapshot = [...items]
  reconcile()
  listeners.forEach((l) => l())
}

/** Start or stop each toast's timer to match what is on screen right now. */
function reconcile() {
  const shown = new Set(items.filter((i) => !i.leaving).slice(-VISIBLE))
  const paused = hovered || hidden
  for (const item of items) {
    const run = !paused && shown.has(item) && Number.isFinite(item.duration) && !item.leaving
    if (run && item.startedAt === null) {
      item.startedAt = Date.now()
      item.timer = setTimeout(() => dismiss(item.id), Math.max(0, item.remaining))
    } else if (!run && item.startedAt !== null) {
      clearTimeout(item.timer)
      item.remaining -= Date.now() - item.startedAt
      item.startedAt = null
    }
  }
}

function remove(key: string) {
  items = items.filter((i) => i.key !== key)
  if (items.length === 0) hovered = false
  commit()
}

/** Dismiss one toast, or all of them with no id. It leaves, then is removed. */
function dismiss(id?: string) {
  for (const item of items) {
    if (item.leaving || (id !== undefined && item.id !== id)) continue
    clearTimeout(item.timer)
    item.startedAt = null
    item.leaving = true
    setTimeout(() => remove(item.key), (REDUCED_MOTION ? FADE : EXIT) * 1000 + 40)
    const onClose = item.onClose
    item.onClose = undefined
    try { onClose?.() } catch (error) { console.error(error) }
  }
  commit()
}

/** Record a toast's measured height so the stack above it sits right. */
function setHeight(key: string, height: number) {
  const item = items.find((i) => i.key === key)
  if (!item) return
  item.height = height
  commit()
}

function show(kind: ToastKind, title: string, options: ToastOptions = {}): string {
  const id = options.id ?? `toast-${nextId++}`
  const duration = options.duration ?? DEFAULT_DURATION
  const existing = items.find((i) => i.id === id && !i.leaving)
  const fields = {
    kind,
    title,
    description: options.description,
    action: options.action,
    cancel: options.cancel,
    icon: options.icon,
    onClose: options.onClose,
    progress: options.progress,
    duration,
    remaining: duration,
  }
  if (existing && options.progress !== undefined && existing.progress !== undefined && existing.kind === kind && existing.title === title) {
    // A progress toast moving on: update the card in place.
    existing.description = options.description
    existing.progress = options.progress
  } else if (existing && existing.kind === kind && existing.title === title && existing.description === options.description && existing.action?.label === options.action?.label && existing.progress === options.progress) {
    // The same toast again: the timer starts over and nothing changes on screen.
    clearTimeout(existing.timer)
    Object.assign(existing, fields, { startedAt: null })
  } else {
    // A new toast, or one that replaces the toast with this id: the old card fades out where it
    // stood while the new one fades in over it.
    let at = items.length
    let height = ESTIMATE
    if (existing) {
      at = items.indexOf(existing)
      height = existing.height
      clearTimeout(existing.timer)
      existing.startedAt = null
      existing.leaving = true
      setTimeout(() => remove(existing.key), (REDUCED_MOTION ? FADE : EXIT) * 1000 + 40)
    }
    items.splice(at, 0, { id, key: `${id}#${nextKey++}`, leaving: false, startedAt: null, height, ...fields })
  }
  commit()
  return id
}

export const toast = Object.assign((title: string, options?: ToastOptions) => show('default', title, options), {
  success: (title: string, options?: ToastOptions) => show('success', title, options),
  info: (title: string, options?: ToastOptions) => show('info', title, options),
  warning: (title: string, options?: ToastOptions) => show('warning', title, options),
  error: (title: string, options?: ToastOptions) => show('error', title, options),
  dismiss,
})

const subscribe = (l: () => void) => {
  listeners.add(l)
  return () => listeners.delete(l)
}

const ICONS: Record<ToastKind, string | null> = {
  default: null,
  success: glyph(CheckmarkCircle02Icon),
  info: glyph(InformationCircleIcon),
  warning: glyph(Alert02Icon),
  error: glyph(AlertCircleIcon),
}
const CLOSE = glyph(Cancel01Icon)

/** Mount once, near the end of the window's tree. Renders nothing, and runs nothing, while no toast shows. */
export function Toaster() {
  const list = useSyncExternalStore(subscribe, () => snapshot)
  return list.length > 0 ? <ToastStack list={list} /> : null
}

function ToastStack({ list }: { list: readonly Item[] }) {
  const size = useWindowSize({ intervalMs: 250 })
  useEffect(() => {
    // Timers pause while the window is hidden (covered, minimized, another Space).
    hidden = !isWindowVisible()
    reconcile()
    const stop = onWindowVisibility((visible) => {
      hidden = !visible
      reconcile()
    })
    return () => {
      stop()
      hidden = false
      hovered = false
    }
  }, [])
  // The newest three stay; a toast that is leaving still renders so its exit plays.
  const newest = new Set(list.filter((i) => !i.leaving).slice(-VISIBLE))
  const visible = list.filter((i) => i.leaving || newest.has(i))
  // Newest at the bottom; each older toast sits above the ones after it.
  let lift = 0
  const placed = [...visible].reverse().map((item) => {
    const entry = { item, lift }
    if (!item.leaving) lift += item.height + GAP
    return entry
  })
  // Sonner stacks collapsed until the pointer enters: the newest card in front, the older ones
  // peeking above it, narrower by 5% per step (the cards carry no content there).
  const live = placed.filter((p) => !p.item.leaving)
  const collapsed = !hovered && live.length > 1
  const x = size.width - NOTIFICATION_INSET
  const base = size.height - NOTIFICATION_INSET
  const layers: Layer[] = placed.map(({ item, lift: above }) => {
    if (!collapsed || item.leaving) return { key: item.key, item, step: 0, x, y: base - above, lift: above, height: item.height }
    const step = live.findIndex((p) => p.item === item)
    if (step === 0) return { key: item.key, item, step: 0, x, y: base, lift: 0, height: item.height }
    return { key: `ghost-${item.key}`, item, step, x, y: base, lift: 0, height: live[0]!.item.height }
  })
  // A layer that vanishes while its toast stays (a card dropping back into the collapsed stack, a
  // ghost stepping forward) fades out where it stood rather than popping; a leaving toast has
  // already faded by itself.
  const seen = useRef(new Map<string, Layer>())
  const retired = useRef(new Map<string, Layer>())
  const [, settle] = useState(0)
  const now = new Set(layers.map((l) => l.key))
  for (const [key, layer] of seen.current) {
    if (now.has(key) || retired.current.has(key) || layer.item.leaving || !items.includes(layer.item)) continue
    retired.current.set(key, layer)
    setTimeout(() => {
      retired.current.delete(key)
      settle((n) => n + 1)
    }, (REDUCED_MOTION ? FADE : EXIT) * 1000 + 60)
  }
  for (const key of now) retired.current.delete(key)
  seen.current = new Map(layers.map((l) => [l.key, l]))
  return (
    <>
      {[...retired.current.values()].map((layer) => (layer.step > 0 ? <GhostLayer key={`out-${layer.key}`} layer={layer} leaving /> : <ToastLayer key={`out-${layer.key}`} layer={{ ...layer, item: { ...layer.item, leaving: true } }} />))}
      {layers.map((layer) => (layer.step > 0 ? <GhostLayer key={layer.key} layer={layer} /> : <ToastLayer key={layer.key} layer={layer} />))}
    </>
  )
}

interface Layer {
  key: string
  item: Item
  /** 0 is a card; an older toast of the collapsed stack is a ghost at 1, 2. */
  step: number
  x: number
  y: number
  /** How far the card sits above the corner: when it changes, the card glides. */
  lift: number
  height: number
}

/**
 * The paint-only enter and exit of one layer: opacity with a blur that clears and a settle from 97%
 * (`.t-toast`). Reduce Motion fades only. `initial` is read once, at mount; a layer that mounts
 * already leaving (a retired layer) starts shown. Undefined without the patched native build, where
 * the layer simply appears.
 */
function useFade(leaving: boolean): PaintTransformProps | undefined {
  const initial = useRef(leaving ? SHOWN() : HIDDEN()).current
  if (!PATCHED_NATIVE) return undefined
  return { initial, animate: leaving ? GONE() : SHOWN(), transition: { duration: leaving ? (REDUCED_MOTION ? FADE : EXIT) : REDUCED_MOTION ? FADE : ENTER, ease: [...(leaving ? EASE.out : EASE.smoothOut)] as [number, number, number, number] } }
}
// Functions, so Reduce Motion is read when a toast moves (it follows System Settings live).
const SHOWN = () => REDUCED_MOTION ? { opacity: 1 } : { opacity: 1, blur: 0, scale: 1 }
const HIDDEN = () => REDUCED_MOTION ? { opacity: 0 } : { opacity: 0, blur: 4, scale: 0.97 }
const GONE = () => REDUCED_MOTION ? { opacity: 0 } : { opacity: 0, blur: 3, scale: 0.985 }

/** An older toast peeking above the front one in the collapsed stack. */
function GhostLayer({ layer, leaving = false }: { layer: Layer; leaving?: boolean }) {
  const t = useTheme()
  const g = useGlass()
  const fade = useFade(leaving)
  const { step, height, x, y } = layer
  const width = WIDTH * (1 - 0.05 * step)
  return (
    <anchored position={{ x: x - (WIDTH - width) / 2, y: y - PEEK * step }} anchor="bottomRight" fit="snap" snapMargin={8} deferred priority={6 - step} occlude backdrop={BACKING} style={{ borderRadius: 10 }}>
      <div onMouseEnter={hoverStack(true)} paintTransform={fade} style={{ width, height, borderRadius: 10, borderWidth: 1, borderColor: g.on ? g.frostRim : t.border, backgroundColor: g.on ? g.denseFloat : t.glass.dense }} />
    </anchored>
  )
}

const hoverStack = (on: boolean) => () => {
  hovered = on
  commit()
}

/**
 * An explicit `{ blur: 0 }` makes the opaque backing GPUIX paints under an anchored surface
 * transparent. The card paints its own opaque surface, so nothing shows through at rest, and while
 * it fades and blurs there is no backing left behind it: the whole layer fades as one.
 */
const BACKING = { blur: 0 }

/**
 * One toast on its own `<anchored>` layer. Each layer is exactly its card, so nothing shows in the
 * gaps between cards. The card cross-fades in and out with `paintTransform` (`useFade`); the layer
 * itself glides when a newer toast pushes it up, or a toast below it leaves.
 */
function ToastLayer({ layer }: { layer: Layer }) {
  const { item, x, y, lift } = layer
  const renderer = useGpuixRequired()
  const card = useRef<PublicInstance>(null)
  const fade = useFade(item.leaving)
  // Gliding after the stack moved: start where this one was and move to its place.
  const [glide, setGlide] = useState<Glide | null>(null)
  const last = useRef(lift)
  if (last.current !== lift) {
    const delta = REDUCED_MOTION || item.leaving ? 0 : lift - last.current
    last.current = lift
    if (delta !== 0) setGlide({ generation: ++generation, from: delta })
  }
  // Measure once painted: the stack needs each card's real height (text wraps).
  useEffect(() => {
    const timer = setTimeout(() => {
      const id = card.current?.id
      const bounds = id != null ? renderer.getElementBounds?.(id) : null
      if (bounds && Math.abs(bounds.height - item.height) > 0.5) setHeight(item.key, bounds.height)
    }, 32)
    return () => clearTimeout(timer)
  }, [renderer, item.key, item.title, item.description, item.height])
  return (
    <anchored
      {...(glide ? { motion: { generation: glide.generation, initial: { top: glide.from }, animate: { top: 0 }, transition: { duration: DURATION_GLIDE, ease: [...EASE.smoothOut] } } } as object : {})}
      style={{ borderRadius: 10 }}
      position={{ x, y }}
      anchor="bottomRight"
      fit="snap"
      snapMargin={8}
      deferred
      priority={6}
      occlude
      backdrop={BACKING}
    >
      <div ref={card} paintTransform={fade} onMouseEnter={hoverStack(true)} onMouseLeave={hoverStack(false)}>
        <ToastCard item={item} />
      </div>
    </anchored>
  )
}

let generation = 2_000_000
interface Glide {
  generation: number
  from: number
}
const DURATION_GLIDE = 0.25

function ToastCard({ item }: { item: Item }) {
  const t = useTheme()
  const g = useGlass()
  const [hover, setHover] = useState(false)
  const icon = item.icon?.source ?? ICONS[item.kind]
  const iconColor = item.icon?.tone ? toneColor(t, item.icon.tone) : t.text
  // A prompt (two buttons): text on top, the buttons on their own trailing row.
  const prompt = !!item.cancel
  const text = (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 2, flexGrow: 1, flexShrink: 1, minWidth: 0 }}>
      <Label color={t.text} size={13} weight={500} lineHeight={20}>
        {item.title}
      </Label>
      {item.description && (
        <Label color={prompt ? t.textSecondary : t.text} size={13} lineHeight={18}>
          {item.description}
        </Label>
      )}
      {item.progress !== undefined && (
        <div style={{ marginTop: 8, width: '100%', height: 4, borderRadius: 2, overflow: 'hidden', backgroundColor: t.variant === 'dark' ? 'rgba(255,255,255,0.14)' : 'rgba(13,13,13,0.1)' }}>
          <div
            paintTransform={PATCHED_NATIVE ? { initial: false, animate: { scaleX: Math.max(0.001, Math.min(1, item.progress)) }, transition: { duration: 0.4, ease: [...EASE.out] as [number, number, number, number] }, origin: [0, 0.5], retain: true } : undefined}
            style={{ width: '100%', height: 4, borderRadius: 2, backgroundColor: t.text }}
          />
        </div>
      )}
    </div>
  )
  return (
    <div
      testId={`toast-${item.kind}`}
      onMouseEnter={() => setHover(true)}
      onMouseLeave={() => setHover(false)}
      // Only the front toast frosts (class `pills`); the ghosts under it are tint-only. No shadow under a translucent fill.
      backdrop={frostBackdrop(g, 'pills', 10)}
      style={{
        display: 'flex',
        flexDirection: prompt ? 'column' : 'row',
        alignItems: prompt ? 'stretch' : 'center',
        gap: prompt ? 12 : 6,
        width: WIDTH,
        padding: 16,
        position: 'relative',
        borderRadius: 10,
        borderWidth: 1,
        borderColor: g.on ? g.pillRim : t.border,
        backgroundColor: frostFill(g, 'pills', t.glass.dense),
        ...(g.on ? {} : { boxShadow: { offsetX: 0, offsetY: 4, blurRadius: 12, spreadRadius: 0, color: 'rgba(0, 0, 0, 0.1)' } }),
      }}
    >
      <GlassEdge radius={10} color={g.frostHighlight} />
      {prompt ? (
        <>
          {/* The icon sits on the title's 20 px line: 2 px down for a 16 px glyph. */}
          <div style={{ display: 'flex', alignItems: 'flex-start', gap: 8 }}>
            {icon && <Icon source={icon} size={16} color={iconColor} style={{ marginTop: 2, flexShrink: 0 }} />}
            {text}
          </div>
          <div style={{ display: 'flex', justifyContent: 'flex-end', alignItems: 'center', gap: 8 }}>
            <Button testId="toast-cancel" size="sm" variant="ghost" onClick={() => { item.cancel!.onClick?.(); dismiss(item.id) }}>{item.cancel!.label}</Button>
            {item.action && <Button testId="toast-action" size="sm" variant="primary" onClick={() => { dismiss(item.id); item.action!.onClick() }}>{item.action.label}</Button>}
          </div>
        </>
      ) : (
        <>
          {icon && <Icon source={icon} size={16} color={iconColor} />}
          {text}
          {item.action && (
            <Button testId="toast-action" size="sm" variant="primary" onClick={() => { item.action!.onClick(); dismiss(item.id) }} style={{ marginLeft: 8 }}>{item.action.label}</Button>
          )}
        </>
      )}
      {/* No swipe-away on a desktop window: a hover close keeps a persistent toast dismissable. */}
      {hover && (
        <div
          testId="toast-close"
          role="button"
          aria-label="Dismiss"
          onClick={() => dismiss(item.id)}
          style={{
            position: 'absolute',
            top: -6,
            left: -6,
            width: 20,
            height: 20,
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'center',
            borderRadius: 10,
            borderWidth: 1,
            borderColor: g.on ? g.pillRim : t.border,
            backgroundColor: g.on ? g.denseFloat : t.popover,
            cursor: 'pointer',
          }}
        >
          <Icon source={CLOSE} size={10} color={t.textSecondary} />
        </div>
      )}
    </div>
  )
}

