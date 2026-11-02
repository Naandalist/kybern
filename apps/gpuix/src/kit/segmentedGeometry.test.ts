import { expect, test } from 'bun:test'
import { fillBoxes } from './segmentedGeometry.ts'

test('fill segments share the track inside its inset and follow its width (ADE-112)', () => {
  const values = ['', 'in_progress', 'failure', 'success']
  expect(fillBoxes(values, 328)).toEqual({ '': { x: 2, w: 81 }, in_progress: { x: 83, w: 81 }, failure: { x: 164, w: 81 }, success: { x: 245, w: 81 } })
  const wide = fillBoxes(values, 488)
  expect(wide.failure).toEqual({ x: 2 + 2 * 121, w: 121 })
  expect(wide.success!.x + wide.success!.w).toBe(488 - 2)
  expect(fillBoxes(['only'], 3)).toEqual({ only: { x: 2, w: 0 } })
})
