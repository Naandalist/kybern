// ADE-118 worst case: long unbreakable URLs and tokens in a user bubble with skill chips, in plain user text, in a short
// inline-code message, in assistant Markdown and in the composer. No daemon connection and no production data; driven by
// `longlink.verify.ts` with GPUIX_BACKGROUND=1.
import '../native/local-build.ts'
import { render } from '@gpuix/react'
import { emptyThreadState, type Block } from '../../../../packages/kybern-client/src/transcript.ts'
import type { ContentPart, ProviderStatus, Thread } from '../../../../packages/kybern-client/src/types.ts'
import { ComposerView } from '../Composer.tsx'
import { onWindowKeyDown } from '../nav/keys.ts'
import { set } from '../state.ts'
import { Transcript } from '../Transcript.tsx'
import { ThemeProvider, useTheme } from '../ui.tsx'
import { TOKEN, URL_A, URL_B } from './longlink.data.ts'

const at = '2026-11-02T15:27:00Z'
const thread: Thread = { id: 'longlink', title: 'Long links', project_id: 'fixture', provider: { kind: 'claude-code', instance: 'default' }, status: 'idle', permission_mode: 'supervised', created_at: at, updated_at: at } as Thread
const skill = (name: string): ContentPart => ({ type: 'skill', name, path: `/skills/${name}/SKILL.md` })
const user = (id: string, seq: number, parts: ContentPart[]): Block => ({ kind: 'user', id, turnId: `turn-${id}`, at, seq, message: { parts } })
const end = (id: string, seq: number): Block => ({ kind: 'turn_end', id: `end-${id}`, turnId: `turn-${id}`, at, seq, stopReason: 'completed', usage: { input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, cache_write_tokens: 0 }, costUsd: null, durationMs: 1000, terminalMessageId: null, error: null })
// Oldest first; the reported case (a long URL among skill chips) is the newest, so it is in view at the bottom.
const blocks: Block[] = [
  user('code', 1, [{ type: 'text', text: `Set \`${TOKEN.slice(0, 60)}\` now` }]),
  { kind: 'assistant', id: 'a1', messageId: 'a1', segment: 0, turnId: 'turn-code', at, seq: 2, origin: { kind: 'root' }, text: `The tracking link ${URL_A} and \`${TOKEN}\` both stay inside the reply.\n\n- ${URL_B}`, thinking: '', complete: true },
  { ...end('code', 3), terminalMessageId: 'a1' } as Block,
  user('plain', 4, [{ type: 'text', text: `Open ${URL_A}\nthen this key: ${TOKEN}` }]),
  end('plain', 5),
  user('chips', 6, [{ type: 'text', text: `Compare ${URL_A} and ${URL_B} for the catalog page. Use ` }, skill('interfaces:better-typography'), { type: 'text', text: ' ' }, skill('interfaces:better-layout'), { type: 'text', text: ' ' }, skill('interfaces:better-writing'), { type: 'text', text: ' ' }, skill('animate'), { type: 'text', text: ' ' }, skill('emil-animations')]),
  end('chips', 7),
]
set({ status: 'open', threads: [thread], selectedId: thread.id, open: { ...emptyThreadState(), loaded: true, thread, blocks } })
const provider = { kind: 'claude-code', available: true, models: [], supported_permission_modes: ['supervised'] } as unknown as ProviderStatus

function Fixture() {
  const t = useTheme()
  return <div testId="fixture-ready" style={{ display: 'flex', flexDirection: 'column', width: '100%', height: '100%', backgroundColor: t.surface }}>
    <Transcript />
    <div style={{ display: 'flex', justifyContent: 'center', padding: 24 }}>
      <ComposerView draftKey="longlink" currentThreadId="longlink-current" provider={{ kind: 'claude-code', instance: 'default' }} providers={[provider]} accounts={[]} model="Claude Opus 5.5" effort="high" mode="supervised" onModeChange={() => {}} catalogLoading={false} onReloadModels={async () => {}} onSend={async () => {}} />
    </div>
  </div>
}
render(<ThemeProvider><Fixture /></ThemeProvider>, { title: 'Long links', width: Number(process.env.KYBERN_LONGLINK_WIDTH ?? 1000), height: 1100, focus: false, onKeyDown: onWindowKeyDown })
process.stdin.on('end', () => process.exit(0))
process.stdin.on('close', () => process.exit(0))
