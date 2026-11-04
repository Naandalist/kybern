// Issue #218: the model picker's search field refocuses on any click in its box, and a large catalog's list is a native
// virtual list whose rows keep their output while the search changes and which follows the arrow keys.
import { expect, mock, test } from 'bun:test'
import { createRequire } from 'node:module'
import { pathToFileURL } from 'node:url'
import path from 'node:path'
import type { ReactNode } from 'react'
import type { ProviderModel, ProviderStatus } from '../../../../packages/kybern-client/src/types.ts'

mock.module('../native/visibility.ts', () => ({ isWindowVisible: () => true, onWindowVisibility: () => () => {} }))
const { ModelPicker } = await import('./ModelPicker.tsx')
const { TextField } = await import('../kit/Field.tsx')
const { ThemeProvider } = await import('../ui.tsx')
const entry = createRequire(import.meta.url).resolve('@gpuix/react')
const { createRoot, flushSync } = await import(pathToFileURL(path.join(path.dirname(entry), 'reconciler/reconciler.js')).href)
const { handleGpuixEvent } = await import(pathToFileURL(path.join(path.dirname(entry), 'reconciler/event-registry.js')).href)

/** Records the element tree (types, test ids, text) and the renderer calls under test, and delivers events. */
function harness() {
  const children = new Map<number, number[]>(), custom = new Map<number, Record<string, any>>(), types = new Map<number, string>()
  const focused: number[] = [], scrolled: [number, number, number | undefined][] = []
  let rootId = 0, created = 0
  const detach = (id: number) => { for (const list of children.values()) { const at = list.indexOf(id); if (at >= 0) list.splice(at, 1) } }
  const renderer = {
    applyBatch(json: string) {
      const gone: number[] = []
      for (const [op, id, a, b] of JSON.parse(json) as [string, number, any, any][]) {
        if (op === 'createElement') { children.set(id, []); custom.set(id, {}); types.set(id, a); created++ }
        else if (op === 'setCustomProp') custom.get(id)![a] = b
        else if (op === 'appendChild') { detach(a); children.get(id)!.push(a) }
        else if (op === 'insertBefore') { detach(a); const list = children.get(id)!; const at = list.indexOf(b); list.splice(at < 0 ? list.length : at, 0, a) }
        else if (op === 'removeChild') detach(a)
        else if (op === 'destroyElement') { detach(id); gone.push(id) }
        else if (op === 'setRoot') rootId = id
      }
      return gone
    },
    setWindowKeyEvents() {}, setWindowSelectionChange() {},
    addLayoutObserver: () => 1, removeLayoutObserver() {}, isLayoutObserverActive: () => true,
    focusElement: (id: number) => { focused.push(id) },
    scrollToItem: (id: number, index: number, offset?: number) => { scrolled.push([id, index, offset]) },
  }
  const root = createRoot(renderer)
  const walk = (id: number, out: number[] = []) => { out.push(id); for (const child of children.get(id) ?? []) walk(child, out); return out }
  const parent = (id: number) => [...children.entries()].find(([, list]) => list.includes(id))?.[0]
  return {
    renderer, focused, scrolled, types, children,
    created: () => created,
    render: (node: ReactNode) => { flushSync(() => root.render(node)) },
    find: (testId: string) => walk(rootId).find(id => custom.get(id)?.testId === testId),
    parent,
    fire: (id: number, payload: Record<string, unknown>) => flushSync(() => handleGpuixEvent({ elementId: id, ...payload }, renderer)),
  }
}

test('a click on the field box focuses its input even after the input reported focus', () => {
  const h = harness()
  h.render(<ThemeProvider><TextField testId="field" label="Search" value="" onChange={() => {}} /></ThemeProvider>)
  const input = h.find('field')!
  // The shell is the input wrapper's parent: <shell><wrapper><input/></wrapper></shell>.
  const shell = h.parent(h.parent(input)!)!
  h.fire(input, { eventType: 'focus' })
  // Off macOS the blur from the press can arrive after the click: the click must still ask for focus.
  h.fire(shell, { eventType: 'click' })
  expect(h.focused).toEqual([input])
  h.fire(input, { eventType: 'blur' })
  h.fire(shell, { eventType: 'click' })
  expect(h.focused).toEqual([input, input])
})

const catalog = (count: number): ProviderModel[] => Array.from({ length: count }, (_, i) => ({
  id: `${['anthropic', 'openai', 'google'][i % 3]}/model-${i}`, display_name: `Model ${i}`, provider: ['anthropic', 'openai', 'google'][i % 3],
  ...(i % 2 ? { description: `Model ${i} description` } : {}),
}))
const picker = (models: ProviderModel[]) => {
  const providers: ProviderStatus[] = [{ kind: 'opencode', display_name: 'OpenCode', available: true, supported_permission_modes: [], supports_fork: false, supports_model_switch: true, models, instances: ['default'] }]
  return (
    <ThemeProvider>
      <ModelPicker provider={{ kind: 'opencode', instance: 'default' }} providers={providers} model={models[0]!.id} canPickModel canPickProvider={false} canReload={false}
        loading={false} busy={false} accounts={[]} onModelChange={async () => true} onEffortChange={async () => true} onProviderChange={async () => true}
        onReload={() => {}} label={() => null} />
    </ThemeProvider>
  )
}
const open = (h: ReturnType<typeof harness>, models: ProviderModel[]) => {
  h.render(picker(models))
  h.fire(h.find('model-picker')!, { eventType: 'mouseDown' })
  h.fire(h.find('model-picker')!, { eventType: 'click' })
  return { search: h.find('model-search')!, list: h.find('model-list')! }
}

test('a large catalog lists its models in a native virtual list; a small one keeps the scroll box', () => {
  const small = harness()
  expect(small.types.get(open(small, catalog(6)).list)).toBe('div')
  const large = harness()
  const { list } = open(large, catalog(600))
  expect(large.types.get(list)).toBe('virtual-list')
  // Top padding, "Agent default", a header and six rows per backend (sections preview), "Show more" per backend, bottom padding.
  expect(large.children.get(list)!.length).toBe(1 + 1 + 3 * (1 + 6 + 1) + 1)
})

test('search results are one item each in the virtual list, and rows that stay listed are not rebuilt', () => {
  const h = harness()
  const { search, list } = open(h, catalog(600))
  h.fire(search, { eventType: 'change', value: 'model 1' })
  const items = h.children.get(list)!.length
  // 150 results (the cap) in three backend sections, the cap note, two paddings.
  expect(items).toBe(2 + 3 + 150 + 1)
  const before = h.created()
  // Every keystroke rebuilds the list's rows; the same results (a trailing space) must not create a single element.
  h.fire(search, { eventType: 'change', value: 'model 1 ' })
  expect(h.children.get(list)!.length).toBe(items)
  expect(h.created() - before).toBe(0)
})

test('the arrow keys scroll the active row into view only when it leaves the list', () => {
  const h = harness()
  const { search, list } = open(h, catalog(600))
  h.fire(search, { eventType: 'change', value: 'model' })
  // Items 0–12 are in view (the top padding, the first header, eleven rows).
  h.fire(list, { eventType: 'visibleRange', startIndex: 0, endIndex: 13 })
  for (let i = 0; i < 5; i++) h.fire(search, { eventType: 'keyDown', key: 'down' })
  expect(h.scrolled).toEqual([])
  for (let i = 0; i < 10; i++) h.fire(search, { eventType: 'keyDown', key: 'down' })
  expect(h.scrolled.length).toBeGreaterThan(0)
  expect(h.scrolled.every(([id]) => id === list)).toBe(true)
  // Moving up past the first row in view scrolls back up, aligning it at the top.
  h.scrolled.length = 0
  h.fire(list, { eventType: 'visibleRange', startIndex: 8, endIndex: 21 })
  for (let i = 0; i < 9; i++) h.fire(search, { eventType: 'keyDown', key: 'up' })
  expect(h.scrolled.at(-1)?.[1]).toBeLessThanOrEqual(8)
})
