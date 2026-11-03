// A Markdown image in an agent reply (`![Caption](.screenshots/shot.png)`). Native Markdown paints images as links, so
// image lines are split out (`splitImages`) and drawn here. A local reference loads through the daemon's
// `/threads/{id}/image` route, as written: relative to the thread's own folder (worktree, Coordinator and delegated
// threads included), or an absolute or `~/` path in any folder (ADE-122). It works the same for remote environments. A source that cannot load shows a card with the caption, the reason and what to do, never a bare
// link; one written moments before the reply finished is retried once. Fresh images resolve from a blur (`ResponseImage`).
import path from 'node:path'
import { useEffect, useState } from 'react'
import { imageSource } from '../../../../packages/kybern-client/src/responseImages.ts'
import { threadClient, useApp } from '../state.ts'
import { useEnvironment } from '../shell/environment.ts'
import { Label, useTheme } from '../ui.tsx'
import { Rise } from '../motion/kit.tsx'
import { Icon } from '../icons.tsx'
import { T } from './glyphs.ts'
import { muted } from './kit.tsx'
import { Button } from '../kit/Button.tsx'
import { ImageLightbox, ResponseImage } from './images.tsx'
import { ImageViewer } from '../motion/ImageMorph.tsx'
import { openTab } from '../tabs/open.ts'
import { storeImageBytes } from './imageFiles.ts'
import { openQuietly, osLabels, revealQuietly } from '../platform/os.ts'

/** Retried once after this, for an image the agent wrote moments before its reply arrived. */
const RETRY_MS = 1500
const CACHE_ENTRIES = 256

/** `source`: the image as a cached file (`imageFiles.ts`), never a `data:` URL. */
type Loaded = { source: string }
type Failure = { status: number; reason: string }
const cache = new Map<string, Loaded>()

function remember(key: string, value: Loaded) {
  cache.delete(key)
  cache.set(key, value)
  while (cache.size > CACHE_ENTRIES) cache.delete(cache.keys().next().value!)
}

/** What the reader is told, by the daemon's answer. */
export function imageFailure(status: number, _text = ''): Failure {
  if (status === 404) return { status, reason: 'Image not found. Retry once the agent has saved it.' }
  // Any folder is served; a 401 or 403 is this device's access to the machine, not the image's place.
  if (status === 401 || status === 403) return { status, reason: 'This device cannot read images on that machine. Pair it again in Settings → Machines.' }
  if (status === 415) return { status, reason: 'Not a PNG, JPEG, GIF, WebP or AVIF image.' }
  if (status === 413) return { status, reason: 'Larger than 50 MB, so it is not shown.' }
  return { status, reason: 'Unable to load the image. Try again.' }
}

/** The reference as the message wrote it: `<…>` destinations unwrapped, `file://` kept for the daemon to read. */
const reference = (source: string) => source.replace(/^<(.*)>$/, '$1')

async function load(threadId: string, source: string, signal: AbortSignal): Promise<Loaded> {
  const owner = threadClient(threadId)
  if (!owner) throw imageFailure(0, '')
  const response = await owner.fetch(`/threads/${encodeURIComponent(threadId)}/image?path=${encodeURIComponent(reference(source))}`, { signal })
  if (!response.ok) throw imageFailure(response.status, await response.text().catch(() => ''))
  const media = response.headers.get('content-type')?.split(';')[0] ?? ''
  if (!media.startsWith('image/')) throw imageFailure(415, '')
  return { source: await storeImageBytes(new Uint8Array(await response.arrayBuffer()), media) }
}

export function useThreadImage(threadId: string | null, source: string) {
  const environment = useEnvironment()
  const key = `${environment.revision}:${threadId}:${source}`
  const [state, setState] = useState<{ key: string; image?: Loaded; failure?: Failure; attempt: number }>(() => ({ key, image: cache.get(key), attempt: 0 }))
  const current = state.key === key ? state : { key, image: cache.get(key), attempt: 0 }
  useEffect(() => {
    if (!threadId) return
    const hit = cache.get(key)
    if (hit) { setState(s => (s.key === key && s.image === hit ? s : { key, image: hit, attempt: 0 })); return }
    const abort = new AbortController(), attempt = current.attempt
    let retry: ReturnType<typeof setTimeout> | undefined
    // `again`: the one automatic retry. It keeps `attempt`, which counts Retry presses only (the effect reloads when it
    // changes, so storing the retry there made a third request).
    const run = (again: boolean) => {
      void load(threadId, source, abort.signal).then(image => {
        if (abort.signal.aborted) return
        remember(key, image)
        setState({ key, image, attempt })
      }, (failure: Failure | Error) => {
        if (abort.signal.aborted) return
        const known = 'reason' in failure ? failure : imageFailure(0, '')
        // Missing, or the connection hiccupped: it may be written moments after the reply. Once.
        if (!again && attempt === 0 && (known.status === 404 || known.status === 0)) { retry = setTimeout(() => run(true), RETRY_MS); return }
        setState({ key, failure: known, attempt })
      })
    }
    run(false)
    return () => { abort.abort(); clearTimeout(retry) }
    // `attempt` changes only through Retry, which reloads.
  }, [key, current.attempt])
  return { ...current, retry: () => setState({ key, attempt: current.attempt + 1 }) }
}

export function ThreadImage({ threadId, source, alt, at }: { threadId: string | null; source: string; alt: string; at?: string }) {
  const target = imageSource(reference(source))
  const label = alt || 'Agent image'
  if (!target) return <ImageCard label={label} reason="This image source cannot be shown." />
  if (target.kind !== 'local') return <ResponseImage source={reference(source)} label={label} at={at} />
  return <LocalThreadImage threadId={threadId} source={source} label={label} at={at} />
}

function LocalThreadImage({ threadId, source, label, at }: { threadId: string | null; source: string; label: string; at?: string }) {
  const image = useThreadImage(threadId, source)
  const cwd = useApp(s => s.threads.find(th => th.id === threadId)?.cwd ?? (s.open.thread?.id === threadId ? s.open.thread?.cwd : undefined))
  const environment = useEnvironment()
  if (image.image) return <ResponseImage source={image.image.source} tabSource={source} label={label} at={at} />
  if (!image.failure) return <ImageCard label={label} loading />
  const local = environment.local && cwd
  const file = local ? path.resolve(cwd, decodeURIComponent(reference(source).replace(/^file:\/\/(localhost)?/, ''))) : null
  // Retry helps only when the file may still appear or the connection failed.
  const retryable = image.failure.status === 404 || image.failure.status === 0 || image.failure.status >= 500
  return <ImageCard label={label} reason={image.failure.reason} onRetry={retryable ? image.retry : undefined}
    onReveal={file ? () => { if (image.failure!.status === 404) openQuietly(path.dirname(file)); else revealQuietly(file) } : undefined} />
}

/** The image's slot while it loads, or why it cannot show: a quiet card with its caption (`ResponseImage`'s width). */
function ImageCard({ label, reason, loading = false, onRetry, onReveal }: { label: string; reason?: string; loading?: boolean; onRetry?: () => void; onReveal?: () => void }) {
  const t = useTheme()
  return <Rise kind="row" play={!loading}>
    <div testId={loading ? 'thread-image-loading' : 'thread-image-failed'} style={{ display: 'flex', flexDirection: 'column', gap: 8, width: 280, marginTop: 12, marginBottom: 12, padding: 12, borderRadius: 12, borderWidth: 1, borderColor: t.border, backgroundColor: t.buttonSecondary }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 8, minWidth: 0 }}>
        <Icon source={T.eye} size={14} color={muted(t)} />
        <Label color={t.text} size={13} weight={500} truncate style={{ flexShrink: 1 }}>{label}</Label>
      </div>
      <Label color={muted(t)} size={12} lineHeight={16}>{loading ? 'Loading image…' : reason ?? ''}</Label>
      {(onRetry || onReveal) && <div style={{ display: 'flex', gap: 8 }}>
        {onRetry && <Button size="sm" testId="thread-image-retry" onClick={onRetry}>Retry</Button>}
        {onReveal && <Button variant="ghost" size="sm" testId="thread-image-reveal" onClick={onReveal}>{osLabels().reveal.replace('Reveal', 'Show')}</Button>}
      </div>}
    </div>
  </Rise>
}

/** An image link clicked in a reply: the image, or the card that says why it cannot load, in the image viewer. */
export function ThreadImagePreview({ threadId, source, onClose }: { threadId: string; source: string; onClose: () => void }) {
  const image = useThreadImage(threadId, source)
  const label = decodeURIComponent(reference(source).split('/').pop() ?? 'Image')
  if (image.image) return <ImageViewer testId="image-lightbox" src={image.image.source} label={label} from={null} source={() => null} onClose={onClose} onOpenInTab={() => openTab(threadId, { kind: 'image', source, label })} />
  return image.failure ? <ImageLightboxCard label={label} reason={image.failure.reason} onClose={onClose} onRetry={image.retry} /> : null
}

function ImageLightboxCard({ label, reason, onClose, onRetry }: { label: string; reason: string; onClose: () => void; onRetry: () => void }) {
  return <ImageLightbox label={label} onClose={onClose} finalFocus={() => {}} body={<ImageCard label={label} reason={reason} onRetry={onRetry} />} />
}
