// The geometry of a `fill` segmented control (`Segmented.tsx`, ADE-112), kept apart so it can be tested without the renderer.

/** The track's inset around its segments (`padding: 2`). */
export const INSET = 2

/**
 * The segment boxes of a `fill` control whose track is `width` wide: equal shares of the space inside the inset (the segments are
 * `flexGrow: 1, flexBasis: 0` with no gap), as offsets from the track's edge, like the measured boxes.
 */
export function fillBoxes(values: readonly string[], width: number): Record<string, { x: number; w: number }> {
  const w = Math.max(0, width - INSET * 2) / Math.max(1, values.length)
  return Object.fromEntries(values.map((value, i) => [value, { x: INSET + i * w, w }]))
}
