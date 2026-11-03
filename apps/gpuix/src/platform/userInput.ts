// When this window last had the user's input (a key or a click). A requested update asks before it restarts a window
// someone is using (`update.ts`); nothing else reads it. Per window: each window is its own process.
let lastInputAt = 0

/** Record input now. Called from the window's key handler and the app root's mouse-down. */
export function markUserInput(now = Date.now()) {
  lastInputAt = now
}

/** Milliseconds since the last input; Infinity when there was none. */
export function sinceUserInput(now = Date.now()): number {
  return lastInputAt === 0 ? Infinity : Math.max(0, now - lastInputAt)
}
