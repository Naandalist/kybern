import { expect, test } from 'bun:test'
import { createRequire } from 'node:module'
import { pathToFileURL } from 'node:url'
import path from 'node:path'
import { useRef, useState } from 'react'
import { TokenTextarea, type TokenTextareaHandle } from './TokenTextarea.tsx'
import { skillReference } from './Completion.tsx'
import { ICON_PAD, tokenText, type TokenReference } from './token-text.ts'
import { textEdit } from '../notes/rich-text.ts'

const entry = createRequire(import.meta.url).resolve('@gpuix/react')
const { createRoot, flushSync } = await import(pathToFileURL(path.join(path.dirname(entry), 'reconciler/reconciler.js')).href)
const { handleGpuixEvent } = await import(pathToFileURL(path.join(path.dirname(entry), 'reconciler/event-registry.js')).href)

/** A renderer that records what the editor writes natively and the `value` React last committed for the textarea. */
function harness() {
  const native = { text: '', edits: [] as string[], selection: null as null | { start: number; end: number }, rejected: 0, reads: 0 }
  let committed: string | null = null
  const renderer = {
    applyBatch(json: string) {
      for (const [op, , a, b] of JSON.parse(json) as [string, number, any, any][]) {
        if ((op === 'setProp' || op === 'setCustomProp') && a === 'value') committed = b
      }
      return []
    },
    setWindowKeyEvents() {}, setWindowSelectionChange() {},
    registerFont() {}, getTextSelection() { return null }, setTextSelection() {},
    getInputSelection() { native.reads++; const at = native.selection ?? { start: native.text.length, end: native.text.length }; return { ...at, reversed: false, composing: false } },
    // The native rule (`input_api.rs` `checked_range`): ordered offsets on UTF-16 character boundaries of the current buffer.
    setInputSelection(_id: number, start: number, end: number) {
      const split = (at: number) => /[\uDC00-\uDFFF]/.test(native.text[at] ?? '') && /[\uD800-\uDBFF]/.test(native.text[at - 1] ?? '')
      if (start > end || end > native.text.length || split(start) || split(end)) { native.rejected++; throw Error('Selection must use ordered UTF-16 character boundaries') }
      native.selection = { start, end }
    },
    editInput(_id: number, edit: { start: number; end: number; text: string }) {
      native.text = native.text.slice(0, edit.start) + edit.text + native.text.slice(edit.end); native.edits.push(edit.text)
      native.selection = { start: edit.start + edit.text.length, end: edit.start + edit.text.length }
    },
    setInputDecorations() {}, hitTestInputDecoration() { return null },
    addLayoutObserver() { return 1 }, removeLayoutObserver() {}, isLayoutObserverActive() { return false },
  }
  const root = createRoot(renderer)
  return { native, committed: () => committed, render: (node: React.ReactNode) => { flushSync(() => root.render(node)) },
    fire: (payload: Record<string, unknown>) => flushSync(() => handleGpuixEvent(payload, renderer)) }
}

const skill = skillReference({ name: 'review-ui', display_name: 'Review UI', path: '/skills/review-ui/SKILL.md', scope: 'user' })
let pick: ((start?: number, end?: number, token?: string) => void) | null = null

/** The composer's shape: the text and the picked chips are state; a pick edits first and registers the chip after. */
function Editor({ initial = '$review' }: { initial?: string }) {
  const [text, setText] = useState(initial)
  const [refs, setRefs] = useState<TokenReference[]>([])
  const ref = useRef<TokenTextareaHandle>(null)
  pick = (start = 0, end = text.length, token = '$review-ui ') => { if (ref.current!.edit(start, end, token, [skill])) setRefs([skill]) }
  return <TokenTextarea testId="composer" editorRef={ref} nativeRef={() => {}} value={text} references={refs} onChange={e => setText(e.value ?? '')} />
}

test('a picked skill is written natively as its chip, and the next render keeps that exact text', () => {
  const h = harness()
  h.native.text = '$review'
  h.render(<Editor />)
  flushSync(() => pick!())
  h.render(<Editor />)
  const chip = `${ICON_PAD}review-ui `
  // Before the fix the native write was the raw `$review-ui ` (the chip was not registered yet), and the decorations of the
  // next render described `  review-ui ` instead: the chip appeared only after the next keystroke re-applied them.
  expect(h.native.edits.at(-1)).toBe(chip)
  expect(h.native.text).toBe(chip)
  expect(h.committed()).toBe(chip)
})

test('a pick replaces only the typed query and leaves the caret right after the chip, even after emoji and before text', async () => {
  const h = harness()
  const before = '👋🏽 hi 🎉 ', after = ' tail'
  h.native.text = `${before}$rev${after}`
  h.native.selection = { start: before.length + 4, end: before.length + 4 }
  h.render(<Editor initial={`${before}$rev${after}`} />)
  // Completion hands the existing space after the caret to the token (`replace` in Completion.tsx).
  flushSync(() => pick!(before.length, before.length + 5, '$review-ui '))
  h.render(<Editor initial={`${before}$rev${after}`} />)
  const chip = `${ICON_PAD}review-ui `
  expect(h.native.text).toBe(`${before}${chip}tail`)
  expect(h.native.edits.at(-1)).toBe(`${ICON_PAD}review-ui`)
  expect(h.native.selection).toEqual({ start: before.length + chip.length, end: before.length + chip.length })
  await Bun.sleep(5)
  expect(h.native.rejected).toBe(0)
  expect(h.native.selection).toEqual({ start: before.length + chip.length, end: before.length + chip.length })
})

test('a caret restore against a native buffer that changed under it is skipped, never thrown', async () => {
  const h = harness()
  h.native.text = '$review'
  h.render(<Editor />)
  flushSync(() => pick!())
  h.render(<Editor />)
  // Typing the native input applied but has not delivered yet: the buffer is shorter than the composer's text.
  h.native.text = '$'; h.native.selection = { start: 1, end: 1 }
  await Bun.sleep(5)
  expect(h.native.selection).toEqual({ start: 1, end: 1 })
})

test('a diff that starts or ends inside a chip is widened to the whole chip', async () => {
  const { snapToChips, tokenText } = await import('./token-text.ts')
  const refs = [{ token: '@a.ts', label: 'a.ts', icon: 'i' }, { token: '@b.ts', label: 'b.ts', icon: 'i' }]
  const previous = tokenText('@a.ts @b.ts x @', refs), next = tokenText('@b.ts x ', refs)
  const bare = textEdit(previous.text, next.text)
  // The bare diff begins after the shared icon room, inside the first chip.
  expect(bare.start).toBeGreaterThan(0)
  const snapped = snapToChips(previous, next, bare)
  expect(snapped.start).toBe(0)
  expect(previous.text.slice(0, snapped.start) + snapped.text + previous.text.slice(snapped.end)).toBe(next.text)
  // Ending inside a chip widens the same way, and a diff clear of chips is untouched.
  const typed = tokenText('@a.ts hi', refs), more = tokenText('@a.ts hi!', refs)
  expect(snapToChips(typed, more, textEdit(typed.text, more.text))).toEqual(textEdit(typed.text, more.text))
})

// Review 2026-10-23 (shared editor code): every programmatic edit (a pick, a pick-again removal, cut, undo, redo) goes through
// `snapToChips`. For any two documents, the widened edit must still produce `next` exactly, never end strictly inside a chip of
// `previous` (the native input would widen it and drop text), and never split a surrogate pair.
test('snapToChips: random edits around adjacent chips, emoji and plain text always give the next text on chip boundaries', async () => {
  const { snapToChips, tokenText } = await import('./token-text.ts')
  const refs = [
    { token: '@a.ts', label: 'a.ts', icon: 'i' }, { token: '@a.tsx', label: 'a.tsx', icon: 'i' }, { token: '@b.ts', label: 'b.ts', icon: 'i' },
    { token: '$review', label: 'review' }, { token: '@image1', label: 'image1', icon: 'i' },
  ]
  const pieces = ['@a.ts', '@a.tsx', '@b.ts', '$review', '@image1', ' ', ' ', '\n', 'x', 'hi', '👍🏽', '😀', 'é', '.', '(']
  let seed = 7
  const rand = (n: number) => { seed = (seed * 1103515245 + 12345) % 2147483648; return seed % n }
  const make = () => Array.from({ length: 1 + rand(8) }, () => pieces[rand(pieces.length)]).join('')
  const inside = (text: string, at: number) => at > 0 && at < text.length && /[\uDC00-\uDFFF]/.test(text[at]!) && /[\uD800-\uDBFF]/.test(text[at - 1]!)
  for (let i = 0; i < 4000; i++) {
    const source = make()
    // An edit of the source the way the composer makes one: delete a range, insert a piece, or both (whole code points only).
    const points = [...source], from = rand(points.length + 1), to = from + rand(points.length + 1 - from)
    const edited = points.slice(0, from).join('') + (rand(2) ? pieces[rand(pieces.length)] : '') + points.slice(to).join('')
    const previous = tokenText(source, refs), next = tokenText(edited, refs)
    const snapped = snapToChips(previous, next, textEdit(previous.text, next.text))
    const result = previous.text.slice(0, snapped.start) + snapped.text + previous.text.slice(snapped.end)
    expect({ source, edited, result }).toEqual({ source, edited, result: next.text })
    for (const span of previous.spans) {
      expect(span.start < snapped.start && snapped.start < span.end).toBe(false)
      expect(span.start < snapped.end && snapped.end < span.end).toBe(false)
    }
    expect(inside(previous.text, snapped.start) || inside(previous.text, snapped.end)).toBe(false)
  }
})

/** The native input here behaves like `input_api.rs`: an `editInput` that intersects an atomic chip widens to the whole chip. */
test('removing one of two adjacent chips, a chip at either end, and re-inserting two keep the native text and the composer in step', () => {
  const native = { text: '', selection: null as null | { start: number; end: number }, atoms: [] as { start: number; end: number }[], widened: 0 }
  let committed: string | null = null, observer: ((error: Error | null, event: unknown) => void) | null = null
  const renderer = {
    applyBatch(json: string) {
      for (const [op, , a, b] of JSON.parse(json) as [string, number, any, any][]) if ((op === 'setProp' || op === 'setCustomProp') && a === 'value') committed = b
      return []
    },
    setWindowKeyEvents() {}, setWindowSelectionChange() {}, registerFont() {}, getTextSelection() { return null }, setTextSelection() {},
    getInputSelection() { const at = native.selection ?? { start: native.text.length, end: native.text.length }; return { ...at, reversed: false, composing: false } },
    setInputSelection(_id: number, start: number, end: number) { native.selection = { start, end } },
    editInput(_id: number, edit: { start: number; end: number; text: string; expectedValue: string }) {
      if (edit.expectedValue !== native.text) throw Error('Input changed before the edit')
      let { start, end } = edit
      // The native rule: a replacement that intersects an atom takes the whole atom.
      for (const atom of native.atoms) if (atom.start < end && start < atom.end && (start > atom.start || end < atom.end)) { start = Math.min(start, atom.start); end = Math.max(end, atom.end); native.widened++ }
      native.text = native.text.slice(0, start) + edit.text + native.text.slice(end)
      native.atoms = []
      native.selection = { start: start + edit.text.length, end: start + edit.text.length }
    },
    setInputDecorations(_id: number, decorations: { start: number; end: number; atomic?: boolean }[], expected: string) {
      if (expected !== native.text) throw Error('Input changed before decorations')
      native.atoms = decorations.filter((d) => d.atomic).map(({ start, end }) => ({ start, end }))
    },
    hitTestInputDecoration() { return null },
    addLayoutObserver(_target: unknown, callback: (error: Error | null, event: unknown) => void) { observer = callback; return 1 },
    removeLayoutObserver() {}, isLayoutObserverActive() { return true },
  }
  const root = createRoot(renderer)
  const refs: TokenReference[] = [{ token: '@a.ts', label: 'a.ts', icon: 'i' }, { token: '@b.ts', label: 'b.ts', icon: 'i' }, { token: '@image1', label: 'image1', icon: 'i' }]
  let handle: TokenTextareaHandle | null = null, text = ''
  function Chips({ initial }: { initial: string }) {
    const [value, setValue] = useState(initial)
    const ref = useRef<TokenTextareaHandle>(null)
    handle = ref.current; text = value
    return <TokenTextarea testId="composer" editorRef={ref} nativeRef={() => {}} value={value} references={refs} onChange={(e) => setValue(e.value ?? '')} />
  }
  const source = '@a.ts @b.ts 👍🏽 x @image1'
  native.text = tokenText(source, refs).text
  flushSync(() => root.render(<Chips initial={source} />))
  // The editor is painted: its decorations (the atoms) are applied natively.
  flushSync(() => observer!(null, { elementId: 1, rect: { x: 0, y: 0, width: 100, height: 20 }, clippedRect: null, frame: 1 }))
  flushSync(() => root.render(<Chips initial={source} />))
  expect(native.atoms.length).toBe(3)
  const step = (start: number, end: number, insert: string, expected: string) => {
    flushSync(() => { expect(handle!.edit(start, end, insert)).toBe(true) })
    flushSync(() => root.render(<Chips initial={source} />))
    flushSync(() => observer!(null, { elementId: 1, rect: { x: 0, y: 0, width: 100, height: 20 }, clippedRect: null, frame: 2 }))
    expect(text).toBe(expected)
    expect(native.text).toBe(tokenText(expected, refs).text)
    expect(committed).toBe(native.text)
  }
  // The first of two adjacent chips, at the start: the labels share their icon room, so the bare diff starts inside a chip
  // and the native input would widen it over the shared prefix (before the fix it ate the neighbour's icon room).
  step(0, 6, '', '@b.ts 👍🏽 x @image1')
  // A chip inserted before another one (the same shared prefix, the other way round).
  step(0, 0, '@a.ts ', '@a.ts @b.ts 👍🏽 x @image1')
  // The second of two adjacent chips.
  step(6, 12, '', '@a.ts 👍🏽 x @image1')
  // The last chip, at the end, and the emoji before it untouched.
  step(13, 20, '', '@a.ts 👍🏽 x ')
  // Two chips inserted back at once, next to each other, and both removed at once.
  step(0, 6, '@b.ts @a.ts ', '@b.ts @a.ts 👍🏽 x ')
  step(0, 12, '', '👍🏽 x ')
  expect(native.widened).toBe(0)
  root.unmount()
})

test('an event that carries its selection is read from the event, not from the native input (#215)', async () => {
  const h = harness()
  h.native.text = 'hello world'
  h.native.selection = { start: 11, end: 11 }
  let id = 0
  const seen: unknown[] = []
  const ref: { current: TokenTextareaHandle | null } = { current: null }
  const node = <TokenTextarea testId="composer" editorRef={ref} nativeRef={element => { if (element) id = element.id }} value="hello world" references={[]}
    onKeyDown={() => seen.push(ref.current!.selection())} onKeyUp={() => seen.push(ref.current!.selection())} onChange={() => {}} />
  h.render(node)
  const at = { start: 5, end: 5, reversed: false, composing: false }
  // The handler and the composer's own read in the same event: one selection, the event's, and no native read.
  h.fire({ elementId: id, eventType: 'keyDown', key: 'left', selection: at })
  h.fire({ elementId: id, eventType: 'keyUp', key: 'left', selection: { ...at, start: 4, end: 4 } })
  expect(seen).toEqual([at, { ...at, start: 4, end: 4 }])
  expect(h.native.reads).toBe(0)
  // An addon that sends no selection (macOS, older builds) is still read natively, in a later event.
  await Promise.resolve()
  h.fire({ elementId: id, eventType: 'keyUp', key: 'left' })
  expect(seen.at(-1)).toEqual({ start: 11, end: 11, reversed: false, composing: false })
  expect(h.native.reads).toBe(1)
})
