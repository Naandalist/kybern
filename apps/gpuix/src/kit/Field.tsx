// One input language (ADE-55): `FieldShell` is the box, `TextField` the single-line input. `SearchField`, `NumberField` and
// `TextArea` build on the shell. 28 high (`lg` 32 in dialogs), radius 9, an ink fill with no edge; hover adds a 1 px ink edge,
// focus a 1.5 px accent edge over a faint accent wash, invalid a 1 px red edge and a 3 px red halo, disabled drops the fill for
// a 1 px ink edge (the spec's 45° hatch needs a repeating pattern the renderer does not have, so it is the edge alone).
// Placeholder text is the theme's `textPlaceholder`, painted over the input (the native one is a fixed grey); the caret is the accent.
import { useGpuix, type InputProps, type PublicInstance, type StyleDesc } from '@gpuix/react'
import { useRef, useState, type ReactNode } from 'react'
import { FONT_MONO, FONT_UI } from '../theme.ts'
import { Label, useTheme } from '../ui.tsx'
import { HEIGHT, RADIUS, useControls } from './controls.ts'

export type FieldSize = 'md' | 'lg'

export interface FieldShellProps {
  size?: FieldSize
  focused?: boolean
  invalid?: boolean
  disabled?: boolean
  /** No box at all (the palette's input): the content only. */
  bare?: boolean
  /** Taller multi-line box (`TextArea`). */
  multiline?: boolean
  width?: number | `${number}%`
  leading?: ReactNode
  trailing?: ReactNode
  children: ReactNode
  testId?: string
  style?: StyleDesc
  /** A click anywhere on the box (its glyph, padding, gaps); `TextField` focuses its input with it. */
  onClick?: () => void
}

export function FieldShell({ size = 'md', focused = false, invalid = false, disabled = false, bare = false, multiline = false, width, leading, trailing, children, testId, style, onClick }: FieldShellProps) {
  const c = useControls()
  const edge = (color: string, spread: number) => ({ offsetX: 0, offsetY: 0, blurRadius: 0, spreadRadius: spread, color, inset: true })
  const shadow = invalid
    ? [edge(c.field.invalid, 1)]
    : focused ? [edge(c.accent, 1.5)] : disabled ? [edge(c.ink(0.06), 1)] : undefined
  return (
    <div
      testId={testId}
      onClick={onClick}
      style={{
        display: 'flex', alignItems: multiline ? 'flex-start' : 'center', gap: 6, minWidth: 0, flexShrink: 1,
        ...(width ? { width } : {}),
        ...(multiline ? { minHeight: 76, paddingTop: 9, paddingBottom: 9 } : { height: size === 'lg' ? HEIGHT.lg : HEIGHT.md }),
        ...(bare ? {} : { paddingLeft: size === 'lg' ? 12 : 10, paddingRight: size === 'lg' ? 12 : 10, borderRadius: RADIUS.field,
          backgroundColor: disabled ? 'rgba(0, 0, 0, 0)' : focused ? c.field.focusWash : c.field.bg,
          ...(shadow ? { boxShadow: shadow } : {}),
          // The 3 px red halo is an outline: a shadow behind the translucent fill would tint it.
          ...(invalid ? { outlineWidth: 3, outlineColor: c.field.invalidRing, outlineOffset: 0 } : {}),
          ...(disabled || focused || invalid ? {} : { hover: { boxShadow: [edge(c.field.hover, 1)] } }) }),
        ...style,
      }}
    >
      {leading}
      {children}
      {trailing}
    </div>
  )
}

export interface TextFieldProps extends Omit<InputProps, 'value' | 'onChange' | 'style' | 'readOnly'> {
  /** Accessible name. */
  label: string
  value: string
  onChange: (value: string) => void
  size?: FieldSize
  width?: number | `${number}%`
  invalid?: boolean
  disabled?: boolean
  bare?: boolean
  mono?: boolean
  leading?: ReactNode
  trailing?: ReactNode
  /** The input's own style (font size) or the shell's (`shell`). */
  style?: StyleDesc
  shell?: StyleDesc
  testId?: string
  /** The input's submit handler (it receives the event: `isPrimary(e.modifiers)` from `platform/keys.ts` for ⌘↵, Ctrl+Enter off macOS). */
  onSubmit?: InputProps['onSubmit']
  onBlur?: () => void
}

export function TextField({ label, value, onChange, size = 'md', width, invalid = false, disabled = false, bare = false, mono = false, leading, trailing, style, shell, testId, onBlur, onFocus, placeholder, ref, ...props }: TextFieldProps) {
  const t = useTheme()
  const { renderer } = useGpuix()
  const [focused, setFocused] = useState(false)
  // The whole box is the field, as in the desktop: a click on its glyph or padding focuses the input (the PR list's
  // narrow search read as a search button that did nothing). A focused input keeps its caret: focusing it again is a no-op.
  // Always ask, rather than skip on a remembered focus: a press on the box moves focus away on mouse down, and off macOS
  // that blur reaches JS after this click (#218), so a flag would still say "focused" and the input would stay blurred.
  const input = useRef<PublicInstance | null>(null)
  const attach = (el: PublicInstance | null) => {
    input.current = el
    if (typeof ref === 'function') ref(el)
    else if (ref) (ref as { current: PublicInstance | null }).current = el
  }
  const focusInput = () => { if (!disabled && input.current) renderer?.focusElement?.(input.current.id) }
  const font = mono ? FONT_MONO : FONT_UI
  const fontSize = typeof style?.fontSize === 'number' ? style.fontSize : size === 'lg' ? 14 : 13
  return (
    <FieldShell size={size} focused={focused} invalid={invalid} disabled={disabled} bare={bare} width={width} leading={leading} trailing={trailing} style={shell} onClick={focusInput}>
      <div style={{ position: 'relative', display: 'flex', flexGrow: 1, flexShrink: 1, minWidth: 0, height: '100%' }}>
        {/* GPUIX paints its own placeholder in a fixed #8f8f8f (2.6:1 on frost, 3.2:1 on white); this one follows the theme. */}
        {placeholder && !value && (
          <div testId={testId ? `${testId}-placeholder` : undefined} style={{ position: 'absolute', top: 0, bottom: 0, left: 0, right: 0, display: 'flex', alignItems: 'center', pointerEvents: 'none' }}>
            <Label color={t.textPlaceholder} size={fontSize} font={font} truncate>{placeholder}</Label>
          </div>
        )}
        <input
          {...props}
          ref={attach}
          testId={testId}
          tabIndex={disabled ? -1 : 0}
          aria-label={label}
          aria-invalid={invalid || undefined}
          value={value}
          readOnly={disabled}
          onChange={(e) => onChange(e.value ?? '')}
          onFocus={(e) => { setFocused(true); onFocus?.(e) }}
          onBlur={() => { setFocused(false); onBlur?.() }}
          style={{ flexGrow: 1, flexShrink: 1, minWidth: 0, height: '100%', borderWidth: 0, fontFamily: font, fontSize, color: disabled ? t.textTertiary : t.text, backgroundColor: 'rgba(0, 0, 0, 0)', ...style }}
        />
      </div>
    </FieldShell>
  )
}
