import { copyFileSync, existsSync, mkdirSync } from 'node:fs'
import { execFileSync } from 'node:child_process'
import path from 'node:path'
import { COMPANION_CFLAGS } from '../scripts/companion-flags.ts'
// The embedded web view host `webview.ts` loads: `build/kybern-webview.dylib` from `webview.m` (WKWebView) on macOS,
// `build/kybern-webview.dll` from the `webview-win` crate (WebView2) on Windows, `build/libkybern-webview.so` from
// `webview-linux.c` (WPE WebKit, WP10) on Linux.
const root = import.meta.dir
mkdirSync(path.join(root, 'build'), { recursive: true })
if (process.platform === 'linux') {
  // Against the private WPE in build/wpe (`wpe/build-wpe.sh build/wpe`) when it exists (what the packages ship;
  // RUNPATH $ORIGIN/wpe/lib), else a system WPE WebKit 2.0 (Debian sid, Fedora) for development.
  const wpe = process.env.KYBERN_WPE_PREFIX ?? path.join(root, 'build/wpe')
  const env = existsSync(path.join(wpe, 'lib/pkgconfig')) ? { ...process.env, PKG_CONFIG_PATH: path.join(wpe, 'lib/pkgconfig') } : process.env
  const flags = execFileSync('pkg-config', ['--cflags', '--libs', 'wpe-webkit-2.0', 'wpe-platform-2.0', 'wpe-platform-headless-2.0'], { env, encoding: 'utf8' }).trim().split(/\s+/)
  execFileSync('cc', ['-shared', '-fPIC', '-O2', '-Wall', '-fvisibility=hidden', '-Wl,-z,defs', '-Wl,-rpath,$ORIGIN/wpe/lib', path.join(root, 'webview-linux.c'), ...flags, '-lz', '-o', path.join(root, 'build/libkybern-webview.so')], { stdio: 'inherit', env })
} else if (process.platform === 'win32') {
  const target = path.join(root, 'build/webview-win-target')
  execFileSync('cargo', ['build', '--release', '--locked', '--manifest-path', path.join(root, 'webview-win/Cargo.toml')], { stdio: 'inherit', env: { ...process.env, CARGO_TARGET_DIR: target, CARGO_INCREMENTAL: '0', RUSTC_WRAPPER: '' } })
  copyFileSync(path.join(target, 'release/kybern_webview.dll'), path.join(root, 'build/kybern-webview.dll'))
} else {
  execFileSync('clang', ['-dynamiclib', ...COMPANION_CFLAGS, '-framework', 'Cocoa', '-framework', 'WebKit', '-framework', 'QuartzCore', '-framework', 'Security', path.join(root, 'webview.m'), '-o', path.join(root, 'build/kybern-webview.dylib')], { stdio: 'inherit' })
  execFileSync('codesign', ['--force', '--sign', '-', path.join(root, 'build/kybern-webview.dylib')], { stdio: 'inherit' })
}
