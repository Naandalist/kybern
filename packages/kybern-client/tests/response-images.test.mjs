import assert from "node:assert/strict"
import test from "node:test"
import { imageSource, responseImages, imageSafeOutput, imageSafeValue, localImageLink, responseImageError } from "../src/responseImages.ts"

test("image URLs retain machine-local paths and reject active or unknown schemes", () => {
  assert.deepEqual(imageSource("screens/first%20pass.png"), { kind: "local", value: "screens/first pass.png" })
  assert.deepEqual(imageSource("file:///workspace/a%20b.png"), { kind: "local", value: "/workspace/a b.png" })
  for (const src of ["javascript:alert(1)", "data:text/html;base64,YQ==", "data:image/svg+xml;base64,YQ==", "file://other/a.png", "//other/a.png", "%zz"]) assert.equal(imageSource(src), null)
})

test("native tool image blocks survive nested harness payloads and deduplicate", () => {
  const blocks = { content: [
    { type: "image", source: { type: "base64", media_type: "image/png", data: "YQ==" } },
    { type: "image", mimeType: "image/png", data: "YQ==" },
    { type: "file", mime: "image/webp", url: "/workspace/result.webp" },
    { type: "imageGeneration", result: "Yg==", outputFormat: "jpeg" },
  ] }
  assert.deepEqual(responseImages(blocks).map((image) => image.source), ["data:image/png;base64,YQ==", "/workspace/result.webp", "data:image/jpeg;base64,Yg=="])
  const safe = JSON.stringify(imageSafeOutput(blocks))
  assert.ok(!safe.includes("YQ==")); assert.ok(!safe.includes("Yg=="))
  assert.ok(safe.includes("/workspace/result.webp"))
})

test("Claude's Read result keeps its picture out of the text view and shows it once", () => {
  // `tool_use_result` repeats the bytes as `file.base64`; the text view printed them as one line of base64.
  const output = { content: [{ type: "image", source: { type: "base64", media_type: "image/png", data: "YQ==" } }], structured: { type: "image", file: { base64: "YQ==", type: "image/png", originalSize: 1, dimensions: { originalWidth: 1 } } } }
  assert.deepEqual(responseImages(output).map((image) => image.source), ["data:image/png;base64,YQ=="])
  for (const text of [JSON.stringify(output, imageSafeValue), JSON.stringify(imageSafeOutput(output))]) {
    assert.ok(!text.includes("YQ=="), text)
    assert.ok(text.includes('"originalSize":1'))
  }
  // A `base64` field that is not an image's stays.
  assert.ok(JSON.stringify({ base64: "Yg==", type: "text/plain" }, imageSafeValue).includes("Yg=="))
})

test("only supported local image links open a thread preview", () => {
  for (const source of ["/workspace/a.png", "artifacts/first%20pass.PNG", "file:///workspace/a%20b.webp", "./result.jpeg"]) assert.equal(localImageLink(source), true, source)
  for (const source of ["https://example.com/a.png", "javascript:alert(1).png", "data:text/html,a.png", "file://other/a.png", "//other/a.png", "page.html", "image.svg", "invalid%zz.png"]) assert.equal(localImageLink(source), false, source)
})

test("only images that can never load are not offered a retry; no folder is refused (ADE-122)", () => {
  assert.equal(responseImageError(new Error("use a PNG, JPEG, GIF, WebP, or AVIF image")).retryable, false)
  assert.doesNotMatch(responseImageError(new Error("Image not found. Check the path, or retry once the agent has saved it.")).message, /copy it into/)
  assert.equal(responseImageError(new Error("Connection interrupted")).retryable, true)
  assert.equal(responseImageError(new Error("image file is unavailable")).retryable, true)
  assert.equal(responseImageError(new Error("images are limited to 50 MB")).retryable, false)
})


test("streaming image-safe JSON matches the old sanitized tree without copying payload objects", () => {
  const output = { nested: [{ type: "image", data: "a".repeat(1000), mimeType: "image/png" }, { type: "imageGeneration", result: "binary", outputFormat: "png" }], text: "plain", values: ["data:image/png;base64,YQ=="], url: "data:image/png;base64,YQ==", stdout: "exact output" }
  assert.equal(JSON.stringify(output, imageSafeValue, 2), JSON.stringify(imageSafeOutput(output), null, 2))
  assert.equal(imageSafeValue.call(output, "nested", output.nested), output.nested)
  assert.equal(output.nested[0].data.length, 1000, "original image remains available to the gallery")
})
