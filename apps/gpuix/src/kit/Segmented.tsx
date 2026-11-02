// The one segmented control (ADE-55, Direction C): a radiogroup on a recessed ink track (28 high, `sm` 24; 2 px inset, radius 11)
// with one raised thumb that slides between segments (radius 9 = track − inset, concentric). The thumb is an accent tint and
// the selected label takes the tinted accent; the others are secondary and light on hover. Labels are 13/500 (`sm` 12) with
// 12 px segment padding; an icon-only option takes 7. `count` trails a label in `text3`. Arrow keys move the choice and the
// focus (roving tab index), skipping `unavailable` options, which stay visible, explain themselves in a tooltip and ignore
// input. While `busy` the control keeps focus but ignores input. Replaces the Settings, Usage, PR-list and Tasks segmented
// controls. `tone="neutral"` is the quiet Tasks style (spec §3.3): an ink thumb and the plain text colour. An icon-only segment
// shows its label in a tooltip. Slide: `.t-tabs-pill` (250 ms smooth-out; the first placement lands without motion; Reduce Motion jumps).
// A `fill` control follows its track's width (ADE-112): the native bounds observer reports it (after paint, then changes only,
// at most once a frame, nothing while idle), the equal segments are computed from it, and a resize moves the thumb without a slide.
import { motion, useGpuixRequired, type PublicInstance, type StyleDesc } from '@gpuix/react'
import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import { observeElementBounds } from '../native/layout.ts'
import { fillBoxes } from './segmentedGeometry.ts'
import { Tip } from '../nav/overlay.tsx'
import { DURATION, EASE } from '../motion/kit.tsx'
import { PATCHED_NATIVE } from '../native/local-build.ts'
import { Icon } from '../icons.tsx'
import { glyphSource } from './Button.tsx'
import { CONTROL, HEIGHT, RADIUS, useControls } from './controls.ts'
import { Label, REDUCED_MOTION, useTheme } from '../ui.tsx'
import { useFocusRing } from './Switch.tsx'

export interface SegmentedOption {
  value: string
  label: string
  /** An icon-only segment: a glyph (kit name or SVG source); `label` is then the accessible name. */
  icon?: string
  /** A count after the label, in `text3`. */
  count?: number | string
  /** Why this option cannot be chosen right now (tooltip); arrow keys skip it. */
  unavailable?: string | null
}

export interface SegmentedProps {
  options: readonly SegmentedOption[]
  value: string
  onChange: (value: string) => void
  busy?: boolean
  'aria-label'?: string
  /** `md` is 28 high (13 px labels), `sm` 24 (12 px). */
  size?: 'sm' | 'md'
  /** Overrides of the size's segment padding (12 / 9). */
  segmentPadX?: number
  /** Stretch to the parent's width with equal segments (`FULL_WIDTH_SEGMENTS`). */
  fill?: boolean
  /** `neutral`: an ink thumb instead of the accent tint (Tasks and Notes). */
  tone?: 'accent' | 'neutral'
  testId?: string
  style?: StyleDesc
}

type Box = { x: number; w: number }

export function Segmented({ options, value, onChange, busy = false, 'aria-label': label, size = 'md', segmentPadX, fill = false, tone = 'accent', testId, style }: SegmentedProps) {
  const t = useTheme()
  const c = useControls()
  const neutral = tone === 'neutral'
  const thumb = neutral ? c.neutral.thumb : c.segmented.thumb
  const selFg = neutral ? c.neutral.selFg : c.segmented.selFg
  const textSize = CONTROL[size].font
  const segmentHeight = (size === 'sm' ? HEIGHT.sm : HEIGHT.md) - 4
  const padX = segmentPadX ?? (size === 'sm' ? 9 : 12)
  const renderer = useGpuixRequired()
  const track = useRef<PublicInstance>(null)
  const buttons = useRef(new Map<string, PublicInstance>())
  const [boxes, setBoxes] = useState<Record<string, Box> | null>(null)
  const [hover, setHover] = useState<string | null>(null)
  const focus = useFocusRing()
  const layoutKey = options.map((o) => `${o.value}:${o.label}:${o.count ?? ''}`).join(',') + `|${textSize}|${padX}|${fill}`
  // `fill`: the track's width from the native observer. Null until it reports, or without the patched build (measured then).
  const [trackWidth, setTrackWidth] = useState<number | null>(null)
  useEffect(() => {
    const node = track.current
    if (!fill || !node) return
    try {
      return observeElementBounds(renderer, node.id, (event) => {
        const width = event.rect?.width
        if (width != null && width > 0) setTrackWidth((last) => (last != null && Math.abs(last - width) < 0.5 ? last : width))
      })
    } catch {
      return undefined
    }
  }, [fill, renderer])

  // Measure each segment after layout (offsets from the track's padding box), retrying until GPUI has laid it out. A changed
  // label or count still reports the previous frame's bounds here, so keep reading until two frames agree.
  useLayoutEffect(() => {
    let tries = 0
    let timer: ReturnType<typeof setTimeout> | undefined
    let last = ''
    const measure = () => {
      const root = track.current ? renderer.getElementBounds?.(track.current.id) : null
      const next: Record<string, Box> = {}
      for (const o of options) {
        const b = buttons.current.get(o.value)
        const r = b ? renderer.getElementBounds?.(b.id) : null
        if (!root || !r || r.width === 0) { if (++tries < 20) timer = setTimeout(measure, 16); return }
        next[o.value] = { x: r.x - root.x, w: r.width }
      }
      const key = JSON.stringify(next)
      if (key === last) return
      last = key
      setBoxes(next)
      if (++tries < 20) timer = setTimeout(measure, 16)
    }
    measure()
    return () => clearTimeout(timer)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [layoutKey])

  const shown = fill && trackWidth != null ? fillBoxes(options.map((o) => o.value), trackWidth) : boxes
  const placed = useRef(false)
  const pill = shown?.[value]
  // The first measured placement does not slide (`data-ready="false"`), and neither does a resize: the thumb stays on its segment.
  const lastWidth = useRef(trackWidth)
  const resized = lastWidth.current !== trackWidth
  lastWidth.current = trackWidth
  const slide = placed.current && !resized && !REDUCED_MOTION
  if (pill) placed.current = true

  const choose = (next: string) => {
    if (busy || next === value || options.find((o) => o.value === next)?.unavailable) return
    onChange(next)
  }
  const step = (dir: number) => {
    if (busy) return
    const index = options.findIndex((o) => o.value === value)
    for (let offset = 1; offset <= options.length; offset++) {
      const next = options[(index + dir * offset + options.length * offset) % options.length]!
      if (next.unavailable || next.value === value) continue
      choose(next.value)
      const node = buttons.current.get(next.value)
      if (node) renderer.focusElement?.(node.id)
      break
    }
  }

  return (
    <div
      ref={track}
      testId={testId}
      role="radiogroup"
      aria-label={label}
      onKeyDown={(e) => {
        const dir = e.key === 'right' || e.key === 'down' ? 1 : e.key === 'left' || e.key === 'up' ? -1 : 0
        if (!dir) return
        e.preventDefault()
        focus.keyed()
        step(dir)
      }}
      style={{
        position: 'relative', display: 'flex', alignItems: 'center', alignSelf: fill ? 'stretch' : 'flex-start', gap: 0, padding: 2, minWidth: 0, flexShrink: fill ? 1 : 0,
        height: segmentHeight + 4, borderRadius: size === 'sm' ? RADIUS.md : RADIUS.segTrack, overflow: 'hidden', backgroundColor: c.segmented.track,
        ...style,
      }}
    >
      {pill && (() => {
        const style = { position: 'absolute', top: 2, height: segmentHeight, borderRadius: size === 'sm' ? RADIUS.sm : RADIUS.segThumb, backgroundColor: thumb, pointerEvents: 'none',
          ...(neutral ? { boxShadow: { offsetX: 0, offsetY: 1, blurRadius: 2, spreadRadius: 0, color: t.variant === 'light' ? 'rgba(0, 0, 0, 0.08)' : 'rgba(0, 0, 0, 0.25)' } } : {}) } as const
        const ease = [...EASE.smoothOut] as [number, number, number, number]
        const thumbId = testId ? `${testId}-thumb` : undefined
        // Equal-width segments (`fill`, or labels that happen to match): the pill only moves, so it slides as a paint-only translation
        // from the first segment's slot (no layout or render per frame). Different widths also change the pill's width, which needs the
        // layout tween.
        const first = shown?.[options[0]!.value]
        if (first && PATCHED_NATIVE && options.every((o) => Math.abs((shown?.[o.value]?.w ?? -1) - pill.w) < 0.5)) {
          return <div testId={thumbId} paintTransform={{ initial: false, animate: { translateX: pill.x - first.x }, transition: { duration: slide ? DURATION.fast : 0, ease } }} style={{ ...style, left: first.x, width: pill.w }} />
        }
        return <motion.div {...(thumbId ? { testId: thumbId } : {})} initial={false} animate={{ left: pill.x, width: pill.w }} transition={{ duration: slide ? DURATION.fast : 0, ease }} style={style} />
      })()}
      {options.map((option) => {
        const selected = option.value === value
        const blocked = !!option.unavailable
        const color = selected ? selFg : hover === option.value && !blocked && !busy ? t.text : blocked ? t.textTertiary : t.textSecondary
        const button = (
          <div
            key={option.value}
            ref={(node: PublicInstance | null) => { if (node) buttons.current.set(option.value, node); else buttons.current.delete(option.value) }}
            testId={testId ? `${testId}-${option.value}` : undefined}
            role="radio"
            aria-label={option.label}
            aria-checked={selected}
            aria-disabled={busy || blocked || undefined}
            tabIndex={selected ? 0 : -1}
            onMouseEnter={() => setHover(option.value)}
            onMouseLeave={() => setHover((h) => (h === option.value ? null : h))}
            onMouseDown={focus.handlers.onMouseDown}
            onFocus={focus.handlers.onFocus}
            onBlur={focus.handlers.onBlur}
            onClick={() => choose(option.value)}
            onKeyDown={(e) => { if (e.key === 'enter' || e.key === 'space') choose(option.value) }}
            style={{
              position: 'relative', display: 'flex', alignItems: 'center', justifyContent: 'center', gap: 6, height: segmentHeight, minWidth: option.icon ? segmentHeight + 8 : 0, paddingLeft: option.icon ? 7 : padX, paddingRight: option.icon ? 7 : padX, borderRadius: size === 'sm' ? RADIUS.sm : RADIUS.segThumb,
              cursor: busy || blocked ? 'default' : 'pointer', ...(fill ? { flexGrow: 1, flexBasis: 0, minWidth: 0 } : { flexShrink: 0 }),
              ...(busy || blocked ? {} : { focusVisible: { outlineWidth: 2, outlineColor: c.focus, outlineOffset: 0 } }),
            }}
          >
            {option.icon
              ? <Icon source={glyphSource(option.icon)} size={14} color={color} style={{ pointerEvents: 'none' }} />
              : <Label color={color} size={textSize} lineHeight={textSize + 4} weight={500} truncate={fill} style={{ pointerEvents: 'none' }}>{option.label}</Label>}
            {option.count != null && <Label color={t.textTertiary} size={textSize} lineHeight={textSize + 4} weight={500} style={{ pointerEvents: 'none' }}>{String(option.count)}</Label>}
          </div>
        )
        if (blocked) return <Tip key={option.value} label={option.unavailable!} side="top" sideOffset={6} size={segmentHeight}>{button}</Tip>
        return option.icon ? <Tip key={option.value} label={option.label} side="bottom" sideOffset={6} size={segmentHeight}>{button}</Tip> : button
      })}
    </div>
  )
}
