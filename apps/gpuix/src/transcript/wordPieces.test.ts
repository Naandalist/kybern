import { expect, test } from 'bun:test'
import { LONG_WORD, PIECE, tightPair, wordPieces } from './wordPieces.ts'

test('short words stay whole', () => {
  expect(wordPieces('catalog')).toEqual(['catalog'])
  expect(wordPieces('x'.repeat(LONG_WORD))).toEqual(['x'.repeat(LONG_WORD)])
})

test('a long URL is cut after its punctuation, and every few characters without any (ADE-118)', () => {
  const url = 'https://l.facebook.com/l.php?u=https%3A%2F%2Fexample.com&h=AT0NDU3NDI4NzQxNAABp8ikLWruahnElB9CFQbwwoS6LJGrH7_aem_fojk3sDjSsxTIAW#sp-catalog'
  const pieces = wordPieces(url)
  expect(pieces.join('')).toBe(url)
  expect(pieces.every((p) => p.length <= PIECE)).toBe(true)
  expect(pieces.slice(0, 3)).toEqual(['https://', 'l.', 'facebook'])
  const key = ['sk', 'live', '51HxQ2bC8aZt7Kq0yLwU4mN9pR3sV6tX1zA2cE5fH8jK0lM3nP6q'].join('_')
  expect(wordPieces(key).join('')).toBe(key)
  expect(wordPieces(key).every((p) => p.length <= PIECE)).toBe(true)
})

test('runs without punctuation are cut between letters that do not kern tightly where they can', () => {
  const run = 'AT0NDU3NDI4NzQxNAABp8ikLWruahnElB9CFQbwwoS6LJGrH7'
  const pieces = wordPieces(run)
  expect(pieces.join('')).toBe(run)
  expect(pieces.every((p) => p.length <= PIECE)).toBe(true)
  // The reviewed gap: "…ikL|Wruahn" split the L–W pair.
  expect(pieces.some((p, i) => p.endsWith('L') && pieces[i + 1]?.startsWith('W'))).toBe(false)
  const tightCuts = pieces.slice(1).filter((p, i) => tightPair(pieces[i]!.at(-1)!, p[0]!))
  expect(tightCuts.length).toBe(0)
})
