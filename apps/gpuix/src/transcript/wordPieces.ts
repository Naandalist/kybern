// Long words in a sent message (ADE-118): the bubble's sentence is a wrapping row of word nodes (`InlineTokens.tsx`), so a
// word longer than a line, a tracking URL or a key, is drawn as short pieces the row can wrap between, its
// `overflow-wrap: anywhere`. Kept apart from the component so it can be tested without the renderer.

/** Words up to this long stay one piece. */
export const LONG_WORD = 20
/** The longest piece of a long word: about 65 px of body text, the most a wrapped line can fall short of its edge. */
export const PIECE = 8

/**
 * Letters that kern tightly with their neighbours (`LW`, `AV`, `To`, `ry`, …). Each piece is its own text node, so a cut
 * between two of them loses the kerning and shows as a gap that reads like a space; cuts avoid them.
 */
const TIGHT = /[AFJKLPTVWXYfjkrtvwxy]/
const LETTER = /[A-Za-z]/
/** Two letters, at least one of which kerns tightly: a cut between them would show. */
export function tightPair(a: string, b: string): boolean {
  return LETTER.test(a) && LETTER.test(b) && (TIGHT.test(a) || TIGHT.test(b))
}

/** A run without punctuation in chunks of at most `PIECE`, each cut moved up to 3 characters earlier to a loose pair. */
function runChunks(run: string): string[] {
  const chunks: string[] = []
  let start = 0
  while (run.length - start > PIECE) {
    let cut = start + PIECE
    for (let at = start + PIECE; at > start + PIECE - 4; at--) {
      if (!tightPair(run[at - 1]!, run[at]!)) { cut = at; break }
    }
    chunks.push(run.slice(start, cut))
    start = cut
  }
  chunks.push(run.slice(start))
  return chunks
}

/**
 * A word cut into pieces the row may wrap between: after URL punctuation (`/`, `?`, `&`, `=`, …), short neighbours merged,
 * and runs without any cut every `PIECE` characters or a little before, between letters that don't kern (`TIGHT`). Joined,
 * the pieces are the word. Short words come back whole.
 */
export function wordPieces(word: string): string[] {
  if (word.length <= LONG_WORD) return [word]
  const pieces: string[] = []
  let current = ''
  for (const part of word.split(/(?<=[/?&=#%\-_.,;:+~@!|\\])/)) {
    for (const chunk of runChunks(part)) {
      if (current && current.length + chunk.length > PIECE) { pieces.push(current); current = '' }
      current += chunk
    }
  }
  if (current) pieces.push(current)
  return pieces
}
