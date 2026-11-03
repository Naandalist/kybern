// Microsoft's Evergreen WebView2 Runtime bootstrapper for the installer (`scripts/package-windows.ts`,
// `installer.nsi` `InstallWebView2`), kept apart so CI can fetch and check it on its own:
//
//   bun packaging/windows/webview2.ts [--cache <dir>]     fetch (or reuse) the pinned bootstrapper and check it
import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from 'node:fs'
import os from 'node:os'
import path from 'node:path'

/** Microsoft's permanent link to the current bootstrapper (`MicrosoftEdgeWebview2Setup.exe`). The file behind it changes. */
export const WEBVIEW2_BOOTSTRAPPER_URL = 'https://go.microsoft.com/fwlink/p/?LinkId=2124703'

/**
 * The bootstrapper every installer carries, pinned by content, so a build is reproducible and never ships a file nobody
 * checked. When Microsoft replaces it the download stops matching and the build fails: check the new file's signature on
 * Windows (`bun packaging/windows/webview2.ts --accept`, which prints its SHA-256 and size), then update these.
 */
export const WEBVIEW2_BOOTSTRAPPER = { sha256: 'cf2f2445fef0c5c14e372045f84998771eeb8bb01d59be4afd00692fa160082e', size: 1_897_688 }

/** Its Authenticode signer: Microsoft Corporation, issued by a Microsoft Code Signing PCA. */
export const WEBVIEW2_SIGNER = {
  subject: 'CN=Microsoft Corporation, O=Microsoft Corporation, L=Redmond, S=Washington, C=US',
  // The 2024 PCA's name has no L= or S= (seen on windows-2022, 2026-10-26); the older PCAs' do.
  issuer: /^CN=Microsoft Code Signing PCA( \d{4})?, O=Microsoft Corporation(, L=Redmond, S=Washington)?, C=US$/,
}

/** SHA-256 and size of a candidate, and whether they are the pinned ones. */
export function pinned(bytes: Uint8Array, pin = WEBVIEW2_BOOTSTRAPPER) {
  const sha256 = createHash('sha256').update(bytes).digest('hex')
  return { sha256, size: bytes.length, ok: sha256 === pin.sha256 && bytes.length === pin.size }
}

/** Whether `Get-AuthenticodeSignature` output (`status|subject|issuer`) is a valid Microsoft signature. */
export function signerMatches(output: string, signer = WEBVIEW2_SIGNER) {
  const [status, subject, issuer] = output.trim().split('|')
  return status === 'Valid' && subject === signer.subject && !!issuer && signer.issuer.test(issuer)
}

/** Where a checked bootstrapper is kept between builds, named by its hash. */
export function defaultCache(platform: NodeJS.Platform = process.platform, env: Record<string, string | undefined> = process.env, home = os.homedir()) {
  const p = platform === 'win32' ? path.win32 : path.posix
  const base = platform === 'win32' ? env.LOCALAPPDATA || p.join(home, 'AppData', 'Local')
    : platform === 'darwin' ? p.join(home, 'Library', 'Caches') : env.XDG_CACHE_HOME || p.join(home, '.cache')
  return p.join(base, 'kybern-gpuix-packaging')
}

function signature(file: string) {
  const script = `$s = Get-AuthenticodeSignature -LiteralPath '${file.replaceAll("'", "''")}'; Write-Output ("{0}|{1}|{2}" -f $s.Status, $s.SignerCertificate.Subject, $s.SignerCertificate.Issuer)`
  return execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], { encoding: 'utf8' })
}

/**
 * The pinned bootstrapper (about 2 MB; it downloads the runtime), for machines without the runtime. Microsoft's
 * redistribution terms allow shipping it. `given` is a copy to use; otherwise the cached copy, else a download into the
 * cache. Its SHA-256 and size must be the pinned ones, always; on Windows (`verify`) its Authenticode signature must also
 * be valid and Microsoft's. Returns the checked file.
 */
export async function webView2Bootstrapper({ given, verify, cache = defaultCache() }: { given?: string; verify: boolean; cache?: string }): Promise<string> {
  const cached = path.join(cache, `MicrosoftEdgeWebview2Setup-${WEBVIEW2_BOOTSTRAPPER.sha256.slice(0, 16)}.exe`)
  let file = given ? path.resolve(given) : cached
  if (!given && !(existsSync(cached) && pinned(readFileSync(cached)).ok)) {
    const response = await fetch(WEBVIEW2_BOOTSTRAPPER_URL)
    if (!response.ok) throw Error(`Could not download the WebView2 bootstrapper (${response.status}). Pass --webview2-bootstrapper <file>, or --no-webview2-bootstrapper`)
    const bytes = new Uint8Array(await response.arrayBuffer())
    const check = pinned(bytes)
    if (!check.ok) {
      throw Error(`Microsoft's WebView2 bootstrapper changed (SHA-256 ${check.sha256}, ${check.size} bytes; pinned ${WEBVIEW2_BOOTSTRAPPER.sha256}). `
        + 'Check its signature on Windows with `bun packaging/windows/webview2.ts --accept`, then update WEBVIEW2_BOOTSTRAPPER in packaging/windows/webview2.ts')
    }
    mkdirSync(cache, { recursive: true })
    writeFileSync(`${cached}.part`, bytes)
    renameSync(`${cached}.part`, cached)
    file = cached
  }
  const check = pinned(readFileSync(file))
  if (!check.ok) throw Error(`${file} is not the pinned WebView2 bootstrapper (SHA-256 ${check.sha256}, ${check.size} bytes; pinned ${WEBVIEW2_BOOTSTRAPPER.sha256})`)
  if (verify) {
    const output = signature(file)
    if (!signerMatches(output)) throw Error(`${file} is not signed by Microsoft (${output.trim()})`)
  }
  console.log(`WebView2 bootstrapper: ${file} (${check.size} bytes, SHA-256 ${check.sha256})`)
  return file
}

if (import.meta.main) {
  const flag = (name: string) => { const at = process.argv.indexOf(name); return at >= 0 ? process.argv[at + 1] : undefined }
  const cache = flag('--cache') ?? defaultCache()
  if (process.argv.includes('--accept')) {
    // A new bootstrapper from Microsoft: download it, require Microsoft's signature, print what to pin.
    if (process.platform !== 'win32') throw Error('Run --accept on Windows: the signature check needs Get-AuthenticodeSignature')
    const response = await fetch(WEBVIEW2_BOOTSTRAPPER_URL)
    if (!response.ok) throw Error(`Could not download the WebView2 bootstrapper (${response.status})`)
    const bytes = new Uint8Array(await response.arrayBuffer())
    const file = path.join(os.tmpdir(), `MicrosoftEdgeWebview2Setup-${Date.now()}.exe`)
    writeFileSync(file, bytes)
    const output = signature(file)
    if (!signerMatches(output)) throw Error(`The download is not signed by Microsoft (${output.trim()})`)
    const { sha256, size } = pinned(bytes)
    console.log(`signed: ${output.trim()}\nWEBVIEW2_BOOTSTRAPPER = { sha256: '${sha256}', size: ${size} }`)
  } else {
    await webView2Bootstrapper({ verify: process.platform === 'win32', cache })
  }
}
