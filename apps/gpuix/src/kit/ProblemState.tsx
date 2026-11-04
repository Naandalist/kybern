// A page-level problem: what went wrong in plain words, the action that fixes it, a secondary action, and the technical
// text behind a "Show details" disclosure with a copy button, never as the headline. The pull request, Actions and
// review surfaces use it for `Problem`s (`packages/kybern-client/src/problem.ts`); copy comes from the caller.
import { useState } from 'react'
import { writeClipboardText } from '../platform/os.ts'
import { FONT_MONO } from '../theme.ts'
import { Label, useTheme } from '../ui.tsx'
import { Button } from './Button.tsx'

const column = { display: 'flex', flexDirection: 'column', minWidth: 0 } as const
const row = { display: 'flex', alignItems: 'center', gap: 8, minWidth: 0 } as const

export interface ProblemStateAction {
  label: string
  onClick: () => void
  testId?: string
}

export interface ProblemStateProps {
  title: string
  body: string
  primary?: ProblemStateAction
  secondary?: ProblemStateAction
  /** The technical error, shown only when the person opens the details. */
  detail?: string
  testId?: string
}

export function ProblemState({ title, body, primary, secondary, detail, testId = 'problem' }: ProblemStateProps) {
  const t = useTheme()
  const [open, setOpen] = useState(false)
  const [copied, setCopied] = useState(false)
  const copy = () => { if (detail) void writeClipboardText(detail).then(() => setCopied(true), () => {}) }
  return (
    <div role="alert" testId={testId} style={{ ...column, gap: 12 }}>
      <Label color={t.text} size={16} weight={500}>{title}</Label>
      <Label color={t.textSecondary} size={13} lineHeight={20}>{body}</Label>
      {(primary || secondary) && (
        <div style={{ ...row, gap: 8 }}>
          {primary && <Button testId={primary.testId ?? `${testId}-primary`} variant="primary" onClick={primary.onClick}>{primary.label}</Button>}
          {secondary && <Button testId={secondary.testId ?? `${testId}-secondary`} variant={primary ? 'secondary' : 'primary'} onClick={secondary.onClick}>{secondary.label}</Button>}
        </div>
      )}
      {detail && (
        <div style={{ ...column, gap: 8 }}>
          <div style={row}>
            <Button testId={`${testId}-details`} size="sm" variant="ghost" chevron onClick={() => setOpen(o => !o)}>{open ? 'Hide details' : 'Show details'}</Button>
            {open && <Button testId={`${testId}-copy`} size="sm" variant="ghost" onClick={copy}>{copied ? 'Copied' : 'Copy details'}</Button>}
          </div>
          {open && <Label color={t.textTertiary} size={12} lineHeight={18} font={FONT_MONO} style={{ userSelect: 'text' }}>{detail}</Label>}
        </div>
      )}
    </div>
  )
}
