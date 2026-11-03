import { toast } from './toasts.tsx'

// GPUIX answers every uncaught throw and unhandled rejection by replacing the whole window with its "Uncaught runtime
// errors" screen. For a throw from a timer or a promise (a caret restore, an RPC nobody awaited) the React tree is intact,
// so that turns one failed side effect into a lost window. Here those are logged with their stack and shown as an error
// toast instead, and the window keeps working. Render errors are left alone: React unmounts the tree on them, so GPUIX's
// screen (with its Reload button) is the right answer there, as it is for throws inside native event dispatch.
// `KYBERN_STRICT_ERRORS=1` keeps GPUIX's screen for these too; fixtures and checks never install this net.

/** GPUIX installs its own handlers only when this key is empty (`installRuntimeErrorHandlers` in @gpuix/react). */
const HANDLERS_KEY = '__gpuixRuntimeErrorHandlers'
const TOAST_INTERVAL_MS = 10_000

let lastToast = 0

type Notify = (title: string, description: string) => void
const toastError: Notify = (title, description) => { toast.error(title, { id: 'runtime-error', description }) }

function report(kind: 'exception' | 'rejection', thrown: unknown, notify: Notify) {
  const error = thrown instanceof Error ? thrown : new Error(String(thrown))
  // A call on a machine that is not connected (`OfflineError`) is not a crash: the thread or row that made it already
  // says the machine is offline, so a second "Something went wrong" toast only alarms (ADE-124).
  if (error.name === 'OfflineError') {
    console.warn(`[kybern] ${kind} from a machine that is not connected:`, error.message)
    return
  }
  console.error(`[kybern] uncaught ${kind} (window kept):`, error.stack ?? error.message)
  const now = Date.now()
  if (now - lastToast < TOAST_INTERVAL_MS) return
  lastToast = now
  try {
    notify('Something went wrong', `${error.message || error.name}. Kybern kept running; if this repeats, restart the app.`)
  } catch (failure) { console.error('[kybern] could not show the error toast:', failure) }
}

export function installRuntimeSafetyNet(notify: Notify = toastError) {
  if (process.env.KYBERN_STRICT_ERRORS === '1' || Reflect.get(globalThis, HANDLERS_KEY)) return
  const handlers = {
    uncaughtException: (error: unknown) => report('exception', error, notify),
    unhandledRejection: (reason: unknown) => report('rejection', reason, notify),
  }
  process.on('uncaughtException', handlers.uncaughtException)
  process.on('unhandledRejection', handlers.unhandledRejection)
  Reflect.set(globalThis, HANDLERS_KEY, handlers)
}
