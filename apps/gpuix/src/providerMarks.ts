// Source geometry: desktop/components/kit/{ProviderIcon,Icons,AntigravityIcon}.
import { fileURLToPath } from 'node:url'
import claude from './assets/claude.svg' with { type: 'text' }
import codex from './assets/openai.svg' with { type: 'text' }
import cursor from './assets/providers/cursor.svg' with { type: 'text' }
import devin from './assets/providers/devin.svg' with { type: 'text' }
import grok from './assets/providers/grok.svg' with { type: 'text' }
import pi from './assets/providers/pi.svg' with { type: 'text' }
import droid from './assets/providers/droid.svg' with { type: 'text' }

export const PROVIDER_VECTORS: Record<string, string> = { 'claude-code': claude, claudeAgent: claude, codex, cursor, devin, grok, 'grok-build': grok, pi, droid }
// GPUI's SVG node is a tinted alpha mask. Keep multicolour marks as transparent
// 128px rasters (4x the largest provider slot), never tint away the source colours.
export const PROVIDER_IMAGES = {
  omp: fileURLToPath(new URL('./assets/providers/omp-colour.png', import.meta.url)),
  antigravity: fileURLToPath(new URL('./assets/providers/antigravity-colour.png', import.meta.url)),
  opencode: {
    light: fileURLToPath(new URL('./assets/providers/opencode-light.png', import.meta.url)),
    dark: fileURLToPath(new URL('./assets/providers/opencode-dark.png', import.meta.url)),
  },
}
