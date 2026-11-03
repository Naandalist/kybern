import { expect, test } from 'bun:test'
import { installRuntimeSafetyNet } from './errors.ts'

const shown: { title: string; description?: string }[] = []
const notify = (title: string, description: string) => { shown.push({ title, description }) }

test('a throw from a timer or promise becomes a logged error toast, and GPUIX does not install its window-wide screen', () => {
  const before = { exception: process.listenerCount('uncaughtException'), rejection: process.listenerCount('unhandledRejection') }
  const logged: unknown[][] = []
  const error = console.error
  console.error = (...args: unknown[]) => { logged.push(args) }
  try {
    installRuntimeSafetyNet(notify)
    // @gpuix/react's `installRuntimeErrorHandlers` returns early when this key is set.
    expect(Reflect.get(globalThis, '__gpuixRuntimeErrorHandlers')).toBeTruthy()
    installRuntimeSafetyNet(notify)
    expect(process.listenerCount('uncaughtException')).toBe(before.exception + 1)
    const handlers = Reflect.get(globalThis, '__gpuixRuntimeErrorHandlers') as { uncaughtException(e: unknown): void; unhandledRejection(e: unknown): void }
    handlers.uncaughtException(new Error('Selection must use ordered UTF-16 character boundaries'))
    handlers.unhandledRejection(new Error('second, within the toast interval'))
    expect(shown).toEqual([{ title: 'Something went wrong', description: 'Selection must use ordered UTF-16 character boundaries. Kybern kept running; if this repeats, restart the app.' }])
    expect(logged.length).toBe(2)
    expect(String(logged[1]![1])).toContain('second, within the toast interval')
  } finally {
    console.error = error
    const handlers = Reflect.get(globalThis, '__gpuixRuntimeErrorHandlers') as { uncaughtException(e: unknown): void; unhandledRejection(e: unknown): void }
    process.off('uncaughtException', handlers.uncaughtException)
    process.off('unhandledRejection', handlers.unhandledRejection)
    Reflect.deleteProperty(globalThis, '__gpuixRuntimeErrorHandlers')
  }
})

test('a call on a machine that is not connected is logged, not shown as something going wrong', async () => {
  const { OfflineError } = await import('../../../../packages/kybern-client/src/machines.ts')
  const seen: string[] = []
  const warn = console.warn
  console.warn = (...args: unknown[]) => { seen.push(String(args[1])) }
  try {
    installRuntimeSafetyNet(notify)
    const handlers = Reflect.get(globalThis, '__gpuixRuntimeErrorHandlers') as { uncaughtException(e: unknown): void; unhandledRejection(e: unknown): void }
    const before = shown.length
    handlers.unhandledRejection(new OfflineError('pc-1', true))
    expect(shown.length).toBe(before)
    expect(seen).toEqual(['This machine is offline'])
  } finally {
    console.warn = warn
    const handlers = Reflect.get(globalThis, '__gpuixRuntimeErrorHandlers') as { uncaughtException(e: unknown): void; unhandledRejection(e: unknown): void }
    process.off('uncaughtException', handlers.uncaughtException)
    process.off('unhandledRejection', handlers.unhandledRejection)
    Reflect.deleteProperty(globalThis, '__gpuixRuntimeErrorHandlers')
  }
})
