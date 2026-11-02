// Sent messages with `$skill`, `@file`, `@note`, `@task`, `@thread` and `@plugin` tokens
// (`components/kybern/InlineToken.tsx`): each reads as an icon plus an accent-coloured,
// weight-500 label with no box, in place in the sentence. Notes, tasks and threads open
// on click. The sentence is a wrapping row of words so the bubble still hugs its text. A word too long for one line (a
// tracking URL, a key) is drawn as short pieces cut after URL punctuation, and every few characters where there is none,
// so the row can wrap inside it, the `overflow-wrap: anywhere` of the desktop (ADE-118); one whole-word item would be the
// row's minimum width and push the text out of the bubble.
import { memo, useMemo, useState } from 'react'
import { inlineTokenLabel, mentionTokenKind, type InlineTokenKind } from '@/lib/inlineTokenLabel'
import { partToken } from '../../../../packages/kybern-client/src/composerTokens.ts'
import { createComposerThreadReference } from '../../../../packages/kybern-client/src/threadReferences.ts'
import { parseKybernMention } from '../../../../packages/kybern-client/src/userInput.ts'
import type { ContentPart } from '../../../../packages/kybern-client/src/types.ts'
import { tokenIcon } from '../composer/token-icons.ts'
import { Icon } from '../icons.tsx'
import { openNote } from '../notes/store.ts'
import { goPage, openThread } from '../nav/history.ts'
import { openTask } from '../tasks/store.ts'
import { FONT_UI } from '../theme.ts'
import { useTheme } from '../ui.tsx'
import { TEXT } from './kit.tsx'
import { LONG_WORD, wordPieces } from './wordPieces.ts'

type Segment = { text: string } | { token: string; kind: InlineTokenKind; label: string; open?: () => void }
const WORD_LIMIT = 400
/** The sentence as text and token segments, as `UserBubble` builds it; null when there is no token to draw. */
export function tokenSegments(parts: readonly ContentPart[]): Segment[] | null {
  const segments: Segment[] = []
  let text = ''
  const flush = () => { if (text) { segments.push({ text }); text = '' } }
  let any = false
  for (const part of parts) {
    if (part.type === 'text') { text += part.text; continue }
    if (part.type === 'attachment' || part.type === 'image') {
      // The composer places a mentioned file right after its `@image1`.
      const match = /(?:^|\s)(@(?:image|file)\d+)$/.exec(text)
      if (match) {
        const label = match[1]!, start = text.length - label.length
        text = text.slice(0, start); flush()
        segments.push({ token: label, kind: 'attachment', label: label.slice(1) }); any = true
      }
      continue
    }
    const token = part.type === 'thread_reference' ? createComposerThreadReference({ id: part.thread_id, title: part.title, project_id: part.project_id ?? '' }, []).token : partToken(part)
    if (!token) continue
    flush(); any = true
    if (part.type === 'thread_reference') segments.push({ token, kind: 'thread', label: inlineTokenLabel('thread', token), open: () => openThread(part.thread_id) })
    else if (part.type === 'skill') segments.push({ token, kind: 'skill', label: inlineTokenLabel('skill', token) })
    else if (part.type === 'mention') {
      const target = parseKybernMention(part.path), kind = mentionTokenKind(part.path)
      segments.push({ token, kind, label: part.display_name ?? part.name, open: target?.kind === 'note' ? () => openNote(target.id) : target?.kind === 'task' ? () => { goPage('tasks'); openTask(target.id) } : undefined })
    } else segments.push({ token, kind: 'file', label: inlineTokenLabel('file', token) })
  }
  flush()
  if (!any) return null
  // Nodes, not words: a long word draws as one node per piece.
  const words = segments.reduce((n, s) => n + ('text' in s ? s.text.split(/\s+/).reduce((k, w) => k + wordPieces(w).length, 0) : 3), 0)
  return words > WORD_LIMIT ? null : segments
}

const Word = ({ text, color, weight }: { text: string; color: string; weight?: number }) =>
  <text style={{ color, fontSize: TEXT.size, lineHeight: TEXT.lineHeight, fontFamily: FONT_UI, whiteSpace: 'nowrap', userSelect: 'text', ...(weight ? { fontWeight: weight } : {}) }}>{text}</text>

/** Words and spaces as separate nodes so a long sentence wraps like running text; a long word as its pieces. */
export function Words({ text, color, weight }: { text: string; color: string; weight?: number }) {
  return <>{text.split(/(\n|[^\S\n]+|\S+)/).filter(Boolean).map((piece, i) => piece === '\n'
    ? <div key={i} style={{ width: '100%', height: 0 }} />
    : piece.length > LONG_WORD && /\S/.test(piece)
      ? wordPieces(piece).map((part, j) => <Word key={`${i}.${j}`} text={part} color={color} weight={weight} />)
      : <Word key={i} text={piece} color={color} weight={weight} />)}</>
}

function Token({ segment }: { segment: Extract<Segment, { token: string }> }) {
  const t = useTheme()
  const [hover, setHover] = useState(false)
  const accent = t.v('--color-accent-blue')
  const [, first = '', rest = ''] = /^(\S*)([\s\S]*)$/.exec(segment.label) ?? []
  const color = hover && segment.open ? t.link : accent
  return <>
    <div role={segment.open ? 'link' : undefined} testId={`inline-token-${segment.kind}`} onClick={segment.open} onMouseEnter={() => setHover(true)} onMouseLeave={() => setHover(false)}
      style={{ display: 'flex', alignItems: 'center', gap: 3.5, cursor: segment.open ? 'pointer' : 'default', flexShrink: 0 }}>
      <Icon source={tokenIcon(segment.kind, segment.token)} size={13.3} color={accent} />
      <Word text={first} color={color} weight={500} />
    </div>
    {rest && <Words text={rest} color={color} weight={500} />}
  </>
}

export const TokenText = memo(function TokenText({ segments }: { segments: Segment[] }) {
  const t = useTheme()
  const body = useMemo(() => segments, [segments])
  return <div testId="token-text" style={{ display: 'flex', flexDirection: 'row', flexWrap: 'wrap', alignItems: 'center' }}>
    {body.map((segment, i) => 'text' in segment ? <Words key={i} text={segment.text} color={t.text} /> : <Token key={i} segment={segment} />)}
  </div>
})
