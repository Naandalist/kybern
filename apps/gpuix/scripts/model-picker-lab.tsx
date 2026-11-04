// The composer's model picker (`src/composer/ModelPicker.tsx`) over a large OpenCode-style catalog (issue #218): one
// shipped `ModelPicker`, only its inputs scripted. `scripts/model-picker-perf.ts` launches it and times typing in the
// search and scrolling the list. Every React commit under the picker is written to stderr as `[lab-commit] <n> <ms>`
// (React's actual render duration), next to the native `[kybern-frame]` log when KYBERN_FRAME_TIMES is set.
//   MODEL_PICKER_LAB_MODELS   catalog size (default 600)
import '../src/native/local-build.ts'
import { render } from '@gpuix/react'
import { Profiler } from 'react'
import { ThemeProvider, useTheme } from '../src/ui.tsx'
import { ModelPicker } from '../src/composer/ModelPicker.tsx'
import type { ProviderModel, ProviderStatus } from '../../../packages/kybern-client/src/types.ts'

const BACKENDS = ['anthropic', 'openai', 'google', 'meta-llama', 'mistralai', 'qwen', 'deepseek', 'x-ai', 'cohere', 'nvidia', 'amazon', 'microsoft']
const FAMILIES = ['sonnet', 'opus', 'haiku', 'gpt', 'o-mini', 'gemini-pro', 'gemini-flash', 'llama', 'mixtral', 'coder', 'chat', 'reasoner', 'grok', 'command', 'nemotron', 'nova', 'phi']

/** A deterministic catalog: `count` models over twelve backends, every other one with a description. */
export function labCatalog(count: number): ProviderModel[] {
  return Array.from({ length: count }, (_, i) => {
    const backend = BACKENDS[i % BACKENDS.length]!
    const family = FAMILIES[(i * 7) % FAMILIES.length]!
    const version = `${1 + (i % 5)}.${(i * 3) % 10}`
    return {
      id: `${backend}/${family}-${version}-${i}`,
      display_name: `${family[0]!.toUpperCase()}${family.slice(1)} ${version} r${i}`,
      provider: backend,
      ...(i % 2 ? { description: `${backend} ${family} for ${i % 3 ? 'everyday coding' : 'long-context work'}` } : {}),
      efforts: ['low', 'medium', 'high'],
      default_effort: 'medium',
      ...(i === 0 ? { is_default: true } : {}),
    }
  })
}

const models = labCatalog(Number(process.env.MODEL_PICKER_LAB_MODELS ?? 600))
const providers: ProviderStatus[] = [{
  kind: 'opencode', display_name: 'OpenCode', available: true, supported_permission_modes: [], supports_fork: false,
  supports_model_switch: true, supports_effort_switch: true, supported_efforts: ['low', 'medium', 'high'], models, instances: ['default'],
}]
let commits = 0

function Lab() {
  const t = useTheme()
  return (
    <div style={{ display: 'flex', width: '100%', height: '100%', backgroundColor: t.surface, alignItems: 'flex-end', justifyContent: 'flex-end', padding: 24 }}>
      <Profiler id="picker" onRender={(_id, _phase, actual) => { console.error(`[lab-commit] ${++commits} ${actual.toFixed(2)}`) }}>
        <ModelPicker
          provider={{ kind: 'opencode', instance: 'default' }}
          providers={providers}
          model={models[3]!.id}
          effort="medium"
          canPickModel
          canPickProvider={false}
          canReload={false}
          loading={false}
          busy={false}
          accounts={[]}
          onModelChange={async () => true}
          onEffortChange={async () => true}
          onProviderChange={async () => true}
          onReload={() => {}}
          label={(color) => <text style={{ color, fontSize: 13 }}>{models[3]!.display_name}</text>}
        />
      </Profiler>
    </div>
  )
}

render(<ThemeProvider><Lab /></ThemeProvider>, { title: 'model-picker-lab', width: 900, height: 720, focus: false })
