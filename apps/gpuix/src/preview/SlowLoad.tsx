// The strip under the Browser's chrome while a forwarded page is still loading over a slow link (ADE-126). It sits in
// the chrome's column like the live-reload notice, so it never covers the page, and it goes away when the load ends.
// Only the stable message is a live region, always mounted so a screen reader hears it once when the strip appears and
// once when it goes; the figures that change every few seconds sit outside it.
import { Label, useTheme } from '../ui.tsx'
import { usePorts } from './ports.ts'
import { forwardOf, SLOW_LOAD_MESSAGE, slowLoadDetail } from './slow.ts'
import { usePreview } from './store.ts'

export function SlowLoadStrip({ threadId }: { threadId: string }) {
  const t = useTheme(), s = usePreview(threadId), entry = s.entries[s.index], ports = usePorts(threadId)
  const detail = entry?.machine ? slowLoadDetail(forwardOf(ports.forwards, entry.machine)?.loading) : null
  return <div testId="preview-slow-load" style={{ display: 'flex', alignItems: 'center', gap: 4, paddingLeft: detail ? 12 : 0, paddingRight: detail ? 12 : 0, height: detail ? 28 : 0, flexShrink: 0, overflow: 'hidden' }}>
    <div testId="preview-slow-load-status" role="status" aria-live="polite" style={{ flexShrink: 0 }}>
      {detail && <Label color={t.textSecondary} size={11}>{`${SLOW_LOAD_MESSAGE} —`}</Label>}
    </div>
    {detail && <div testId="preview-slow-load-detail" style={{ flexShrink: 1, minWidth: 0 }}><Label color={t.textSecondary} size={11} truncate>{detail}</Label></div>}
  </div>
}
