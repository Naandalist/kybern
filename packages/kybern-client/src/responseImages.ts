export interface ResponseImage { source: string; label: string }
const MIME = /^image\/(png|jpeg|gif|webp|avif)$/i
export function imageSource(source: string): { kind: "remote" | "inline" | "local"; value: string } | null {
  if (source.length > 70_000_000) return null
  if (/^data:image\/(png|jpeg|gif|webp|avif);base64,[a-z0-9+/=\s]+$/i.test(source)) return { kind: "inline", value: source }
  if (/^https?:\/\//i.test(source)) return { kind: "remote", value: source }
  if (/^file:\/\//i.test(source)) {
    try { const url = new URL(source); return !url.hostname || url.hostname === "localhost" ? { kind: "local", value: decodeURIComponent(url.pathname) } : null } catch { return null }
  }
  if (/^[a-z][a-z0-9+.-]*:/i.test(source) || source.startsWith("//") || !source.trim()) return null
  try { return { kind: "local", value: decodeURIComponent(source) } } catch { return null }
}
export function responseImages(value: unknown): ResponseImage[] {
  const found = new Map<string, ResponseImage>()
  const visit = (value: unknown, depth: number) => {
    if (!value || typeof value !== "object" || depth > 10 || found.size >= 32) return
    if (Array.isArray(value)) { value.forEach((item) => visit(item, depth + 1)); return }
    const o = value as Record<string, unknown>
    const raw = o.source && typeof o.source === "object" ? o.source as Record<string, unknown> : o
    const mime = String(raw.mimeType ?? raw.media_type ?? raw.mime ?? "")
    let source = ""
    if (o.type === "image" && typeof raw.data === "string" && MIME.test(mime)) source = `data:${mime};base64,${raw.data}`
    else if (MIME.test(mime) && typeof raw.url === "string") source = raw.url
    // Claude's Read result repeats the picture as `file: { base64, type }`; the same bytes give the same source, so it is shown once.
    else if (typeof raw.base64 === "string" && MIME.test(String(raw.type ?? ""))) source = `data:${raw.type};base64,${raw.base64}`
    else if (o.type === "image" && raw.type === "url" && typeof raw.url === "string") source = raw.url
    else if (o.type === "imageGeneration" && typeof o.result === "string") source = `data:image/${o.outputFormat === "jpeg" || o.outputFormat === "webp" ? o.outputFormat : "png"};base64,${o.result}`
    else if ((o.type === "imageView" || o.type === "imageGeneration") && typeof o.path === "string") source = o.path
    if (source && imageSource(source)) found.set(source, { source, label: String(o.filename ?? o.title ?? "Agent image") })
    for (const [key, child] of Object.entries(o)) if (!["data", "result", "base64"].includes(key) || typeof child === "object") visit(child, depth + 1)
  }
  visit(value, 0)
  return [...found.values()]
}

/** JSON replacer: omit image bytes without cloning the whole result first. */
export function imageSafeValue(this: unknown, key: string, child: unknown): unknown {
  const record = this as Record<string, unknown> | null
  if (!Array.isArray(record) && typeof child === "string" && isImageBytes(record, key, child)) return "[Image shown above]"
  return child
}

/** A string that is an image's bytes: a `data:` URL, base64 `data`, a generated image's `result`, or Claude's Read result
 *  `file.base64` (`{ base64, type: "image/png" }`), which otherwise reached the text view as one line of base64 megabytes long. */
function isImageBytes(record: Record<string, unknown> | null, key: string, child: string): boolean {
  return child.startsWith("data:image/") || (key === "data" && (record?.type === "base64" || record?.type === "image")) || (key === "result" && record?.type === "imageGeneration")
    || (key === "base64" && MIME.test(String(record?.type ?? "")))
}

/** Keep binary payloads out of the expandable text view; the gallery displays them. */
export function imageSafeOutput(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(imageSafeOutput)
  if (!value || typeof value !== "object") return value
  const record = value as Record<string, unknown>
  return Object.fromEntries(Object.entries(record).map(([key, child]) => {
    if (typeof child === "string" && isImageBytes(record, key, child)) return [key, "[Image shown above]"]
    return [key, imageSafeOutput(child)]
  }))
}

/** Only supported raster-image file links enter the image preview route. */
export function localImageLink(source: string): boolean {
  const target = imageSource(source)
  return target?.kind === "local" && /\.(?:png|jpe?g|gif|webp|avif)$/i.test(target.value)
}

export function responseImageError(error: unknown): { message: string; retryable: boolean } {
  const message = error instanceof Error ? error.message : "Unable to load image. Try again."
  if (message.startsWith("Preview unavailable")) return { message, retryable: false }
  if (message === "use a PNG, JPEG, GIF, WebP, or AVIF image" || message === "images are limited to 50 MB") return { message, retryable: false }
  return { message, retryable: true }
}
