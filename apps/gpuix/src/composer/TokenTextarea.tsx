import { useGpuixRequired, type PublicInstance, type TextareaProps } from '@gpuix/react'
import { useImperativeHandle, useRef, useEffect, type Ref } from 'react'
import { DecoratedTextarea } from '../native/decorated-input.tsx'
import { nativeCapabilities, type InputSelection } from '../native/api.ts'
import { useTheme } from '../ui.tsx'
import { FONT_MONO } from '../theme.ts'
import { alpha, tinted } from '../transcript/kit.tsx'
import { RichHistory, textEdit } from '../notes/rich-text.ts'
import { editTokenText, snapToChips, tokenDisplayOffset, tokenSourceOffset, tokenText, type TokenReference, type TokenText } from './token-text.ts'
import { writeClipboardText, writeClipboardTextSync } from '../platform/os.ts'

export interface TokenTextareaHandle {
  selection(): InputSelection
  /** `added`: the chips this edit introduces. The caller registers them in `references` only on its next render, so the
   * native text is written with them already applied; otherwise it holds the raw token while the decorations (computed
   * from the next render's references) no longer match it, and the chip shows only after the next keystroke. */
  edit(start: number, end: number, text: string, added?: readonly TokenReference[]): boolean
}
interface Props extends TextareaProps {
  testId: string
  value: string
  references: readonly TokenReference[]
  editorRef: Ref<TokenTextareaHandle>
  nativeRef: Ref<PublicInstance>
  onEditorError?: (message: string) => void
  /** A source range painted with the accent wash (the run the secret catcher caught); ignored where it touches a chip. */
  wash?: { start: number; end: number } | null
}
const historyDoc = (text: string) => ({ text, marks: [], lines: [] })
/** Inline labels are native atomic runs. Offset conversion is confined here;
 * completion queries, clipboard serialization and wire parts use raw tokens. */
export function TokenTextarea({ references, editorRef, nativeRef, onEditorError, wash, ...props }: Props) {
  const t = useTheme()
  const accent = t.v('--color-accent-blue')
  const renderer = useGpuixRequired()
  const node = useRef<PublicInstance | null>(null)
  const projection = tokenText(props.value, references)
  const live = useRef(projection)
  live.current = projection
  const history = useRef(new RichHistory({ doc: historyDoc(props.value), start: props.value.length, end: props.value.length }))
  const nativeReplacement = useRef<string | null>(null)
  const lastEmitted = useRef(props.value)
  /** Values this editor emitted that the host has not committed back yet. A key handled before the host's render (Ctrl+Z
   * right after a Backspace, off macOS) emits again, and the late commit of the earlier value is this editor's own edit,
   * not an outside change: recording it would drop the history after it. */
  const uncommitted = useRef<string[]>([])
  const caret = useRef<Pick<InputSelection, 'start' | 'end' | 'reversed'> | null>(null)
  const composing = useRef(false)
  const lastRange = useRef<InputSelection | null>(null)
  // One native read per event: this editor and the composer both read the range in the same key and change handlers.
  const eventRange = useRef<{ id: number; range: InputSelection } | null>(null)
  /** The native range. Off macOS it is a waited UI-thread call, which throws when that thread stays busy for 2 s; the
   * key and change handlers read it, so a throw would drop the key and surface as an uncaught error: use the last range. */
  const nativeSelection = (id: number): InputSelection => {
    if (eventRange.current?.id === id) return eventRange.current.range
    let range: InputSelection
    try {
      range = lastRange.current = nativeCapabilities(renderer).getInputSelection(id)
    } catch (error) {
      console.error(error)
      // Kept for the rest of this event too: a second read would wait out the busy UI thread again.
      range = lastRange.current ?? { start: live.current.text.length, end: live.current.text.length, composing: false, reversed: false }
    }
    const read = eventRange.current = { id, range }
    queueMicrotask(() => { if (eventRange.current === read) eventRange.current = null })
    return range
  }
  /** Off macOS the addon sends each input event with the selection as of that event (#215): the reads of this event (this
   * editor's and the composer's handlers) use it instead of asking the UI thread. Older addons and macOS send none. */
  const adopt = (event: object) => {
    const range = (event as { selection?: InputSelection | null }).selection, id = node.current?.id
    if (!range || id == null) return
    lastRange.current = range
    const read = eventRange.current = { id, range }
    queueMicrotask(() => { if (eventRange.current === read) eventRange.current = null })
  }
  const read = (): InputSelection => {
    if (!node.current) return { start: 0, end: 0, composing: false, reversed: false }
    const range = nativeSelection(node.current.id), doc = live.current
    return { ...range, start: tokenSourceOffset(doc, range.start), end: tokenSourceOffset(doc, range.end, true) }
  }
  const emit = (value: string, at: number, typed: boolean, record = true, refs: readonly TokenReference[] = references) => {
    lastEmitted.current = value; uncommitted.current.push(value)
    if (uncommitted.current.length > 32) uncommitted.current.shift()
    caret.current = { start: at, end: at, reversed: false }
    live.current = tokenText(value, refs)
    if (record) history.current.record({ doc: historyDoc(value), start: at, end: at }, typed)
    props.onChange?.({ value, elementId: node.current?.id ?? 0, eventType: 'change' })
  }
  /** The native range for a source selection in `doc`, or null when `doc` cannot address it (a caret inside a surrogate
   * pair or past the end): the native input rejects those, so they are skipped instead of thrown. */
  const nativeRange = (doc: TokenText, selection: Pick<InputSelection, 'start' | 'end'>) => {
    const start = tokenDisplayOffset(doc, selection.start), end = tokenDisplayOffset(doc, selection.end, true)
    const inside = (at: number) => at > 0 && at < doc.text.length && /[\uDC00-\uDFFF]/.test(doc.text[at]!) && /[\uD800-\uDBFF]/.test(doc.text[at - 1]!)
    return start <= end && end <= doc.text.length && !inside(start) && !inside(end) ? { start, end } : null
  }
  const install = (value: string, at: number, record: boolean, selection?: Pick<InputSelection, 'start' | 'end' | 'reversed'>, added: readonly TokenReference[] = []) => {
    if (!node.current || props.readOnly || read().composing) return false
    const refs = added.length ? [...references, ...added] : references
    const previous = live.current, next = tokenText(value, refs), id = node.current.id
    // Only the changed range is replaced, so the native caret already ends after the inserted text (a picked chip).
    const change = snapToChips(previous, next, textEdit(previous.text, next.text))
    nativeReplacement.current = next.text
    eventRange.current = null
    nativeCapabilities(renderer).editInput(id, { ...change, expectedValue: previous.text })
    const wanted = selection ?? { start: at, end: at, reversed: false }
    emit(value, at, false, record, refs)
    caret.current = wanted
    // `editInput` is synchronous, so the native buffer is `next.text` right now: place the caret against it here rather
    // than from a timer that can run before or after the host's own value write.
    const range = nativeRange(next, wanted)
    if (range) { nativeCapabilities(renderer).setInputSelection(id, range.start, range.end, wanted.reversed); eventRange.current = null }
    return true
  }
  useImperativeHandle(editorRef, () => ({ selection: read, edit: (start, end, text, added) => {
    const value = live.current.source
    return install(value.slice(0, start) + text + value.slice(end), start + text.length, true, undefined, added)
  } }))
  useEffect(() => {
    const own = uncommitted.current.indexOf(props.value)
    if (own >= 0) uncommitted.current.splice(0, own + 1)
    else if (props.value !== lastEmitted.current) {
      lastEmitted.current = props.value
      history.current.record({ doc: historyDoc(props.value), start: props.value.length, end: props.value.length }, false)
    }
    if (caret.current != null && node.current) {
      const wanted = caret.current, id = node.current.id, expected = props.value
      // The host's value write for this commit can move the native caret again; put it back once that has landed. The
      // range is computed when the timer runs, against the text the editor holds then, and only applied if it moved.
      const restore = setTimeout(() => {
        if (node.current?.id !== id || live.current.source !== expected) return
        const now = read()
        if (now.composing || (now.start === wanted.start && now.end === wanted.end)) return
        const range = nativeRange(live.current, wanted)
        if (!range) return
        // A native buffer that still differs from `live` (typing not yet delivered) rejects the range; that keystroke's
        // own change places the caret, so a rejection here must never become an uncaught error.
        eventRange.current = null
        try { nativeCapabilities(renderer).setInputSelection(id, range.start, range.end, wanted.reversed) } catch {}
      }, 0)
      caret.current = null
      return () => clearTimeout(restore)
    }
  }, [props.value, projection.text])
  const rememberSelection = () => {
    const selection = read()
    history.current.selection(selection.start, selection.end, selection.reversed)
  }
  return <DecoratedTextarea {...props} value={projection.text}
    ref={element => { node.current = element; if (typeof nativeRef === 'function') nativeRef(element); else if (nativeRef) nativeRef.current = element }}
    decorations={[...projection.spans.map((span, index) => {
      const icon = references.find(r => r.token === span.token)?.icon
      return { start: span.start, end: span.end, atomic: true, id: `token:${index}:${span.token}`, style: { color: accent, fontWeight: 500 }, ...(icon ? { icon: { svg: tinted(icon, accent), color: '', font: FONT_MONO, size: 13.3 } } : {}) }
    }), ...washDecoration(projection, wash, accent)]}
    interceptKeys={[...new Set([...(props.interceptKeys ?? []), 'c', 'x', 'z', 'y', 'backspace', 'delete'])]}
    onChange={event => {
      adopt(event)
      const value = event.value ?? ''
      if (nativeReplacement.current === value) { nativeReplacement.current = null; return }
      const previous = live.current, range = node.current ? nativeSelection(node.current.id) : null
      const next = editTokenText(previous, value)
      const interim = tokenText(next.value, references)
      const at = range ? tokenSourceOffset(interim, range.start, true) : next.caret
      // Ordinary native typing has already placed the caret; no second write.
      emit(next.value, at, !composing.current && Math.abs(value.length - previous.text.length) === 1, !range?.composing)
      composing.current = !!range?.composing
      caret.current = null
    }}
    onClick={event => { adopt(event); rememberSelection(); props.onClick?.(event) }}
    onKeyUp={event => {
      adopt(event)
      const range = read()
      if (composing.current && !range.composing) {
        composing.current = false
        history.current.record({ doc: historyDoc(live.current.source), ...range }, false)
      }
      rememberSelection()
      props.onKeyUp?.(event)
    }}
    onKeyDown={event => {
      adopt(event)
      const range = read(), mod = event.modifiers?.cmd || event.modifiers?.ctrl
      const saved = history.current.entries[history.current.index]!
      if ((event as typeof event & { requestId?: number }).requestId != null && (saved.start !== range.start || saved.end !== range.end || saved.reversed !== range.reversed)) history.current.selection(range.start, range.end, range.reversed)
      if (!range.composing && mod && !event.modifiers?.alt) {
        if ((event.key === 'c' || event.key === 'x') && range.start !== range.end) {
          event.preventDefault()
          const source = live.current.source, selected = source.slice(range.start, range.end), cut = event.key === 'x' && !props.readOnly
          const finish = () => { if (cut && live.current.source === source) install(source.slice(0, range.start) + source.slice(range.end), range.start, true) }
          if (process.platform === 'darwin') {
            try { writeClipboardTextSync(selected) } catch { onEditorError?.('Unable to copy. Try the keyboard shortcut again.'); return }
            finish()
          } else {
            // A clipboard tool is a process spawn off macOS (up to a second): copy without blocking the UI, and cut only once the
            // text is on the clipboard (and only if the field still holds what was copied from).
            void writeClipboardText(selected).then(finish, () => onEditorError?.('Unable to copy. Try the keyboard shortcut again.'))
          }
          return
        }
        if (!props.readOnly && (event.key === 'z' || (event.key === 'y' && event.modifiers?.ctrl))) {
          event.preventDefault()
          const entry = history.current.restore(event.key === 'y' || event.modifiers?.shift ? 1 : -1)
          if (entry) install(entry.doc.text, entry.start, false, { start: entry.start, end: entry.end, reversed: !!entry.reversed })
          return
        }
      }
      props.onKeyDown?.(event)
    }}
  />
}

/** The wash behind a caught run (`acc .16`, radius 4), in display offsets; none when it would overlap a chip. */
function washDecoration(doc: TokenText, range: { start: number; end: number } | null | undefined, accent: string) {
  if (!range || range.end <= range.start || range.end > doc.source.length) return []
  const start = tokenDisplayOffset(doc, range.start), end = tokenDisplayOffset(doc, range.end, true)
  if (end <= start || doc.spans.some(span => start < span.end && span.start < end)) return []
  return [{ start, end, id: 'secret-catch', style: { backgroundColor: alpha(accent, 0.16), borderRadius: 4 } }]
}
