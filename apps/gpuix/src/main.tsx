// Kybern on GPUIX: the desktop client's look, painted by Zed's GPUI instead of
// a web view. Run with `pnpm dev` (hot reload) or `pnpm start`.
// Must stay the first import: it picks the native module before @gpuix loads.
import './native/local-build.ts'
import { startupMark } from './perf/startup.ts'
import { fstatSync, readFileSync, writeFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { render, useGpuixRequired } from '@gpuix/react'
import { useEffect, useState, type ReactNode } from 'react'
import { onWindowKeyDown } from './nav/keys.ts'
import { markUserInput } from './platform/userInput.ts'
import { appearance } from './settings/appearance.ts'
import { connect } from './state.ts'
import { startSplitView } from './split/store.ts'
import { ThemeProvider, VARIANT } from './ui.tsx'
import { startPerfProbe } from './perf/probe.ts'
import { startPickHub } from './pick/hub.ts'
import { getAccessibilityDisplay } from './platform/material.ts'
import { resolveScope, type ThemeVariant } from './platform/translucency.ts'
import { backingFor, installWindowMaterial, nativeBackground } from './shell/windowMaterial.ts'
import { startAppUpdateChecks } from './platform/update.ts'
import { installFrameSource } from './native/frame.ts'
import { handbackOnFocus, installFocusHandback } from './native/focusHandback.ts'
import { hasSavedFrame, installWindowFrame, startWindowShell } from './shell/window.ts'
import { savedFrameOptions, startWindowChrome } from './shell/windowChrome.ts'
import { startEnvironmentWindow } from './windows/window.ts'
import { installRuntimeSafetyNet } from './shell/errors.ts'
import { startMemoryLog } from './perf/memoryLog.ts'
import { setOsHost } from './platform/os.ts'
import { attachNotificationHost, setEarlyAppIdentity } from './native/notifications.ts'
import { installUiFont } from './platform/fonts.ts'

// The terminal draws xterm's weights (300 text, 500 bold); italics are their own family until GPUIX styles take `fontStyle`.
const FONTS = ['CalSans-Regular.ttf', 'JetBrainsMono-Regular.ttf', 'JetBrainsMono-Light.ttf', 'JetBrainsMono-Medium.ttf', 'JetBrainsMono-LightItalic.ttf', 'JetBrainsMono-MediumItalic.ttf']

function Fonts({ children }: { children: ReactNode }) {
  const renderer = useGpuixRequired()
  const [ready, setReady] = useState(false)
  useEffect(() => {
    startupMark('fonts-start')
    // Windows and Linux use the addon's clipboard, open and picker bindings when it has them (`platform/os.ts`).
    setOsHost(renderer)
    // Windows and Linux post system notifications through the same renderer (`native/notifications.ts`).
    attachNotificationHost(renderer)
    installFrameSource(renderer)
    // Focus that GPUIX moves takes the keyboard back from a clicked web document (`native/focusHandback.ts`).
    installFocusHandback(renderer)
    // Point for me: the picker in the Preview and visuals, and ⌥-click anchors (`pick/hub.ts`).
    startPickHub()
    // `registerFont` landed after @gpuix/native 0.10.0; until then the UI
    // falls back to installed fonts (SF Pro, Menlo).
    const register = (renderer as { registerFont?: (bytes: Uint8Array) => void }).registerFont
    for (const file of register ? FONTS : []) {
      try {
        register!.call(renderer, readFileSync(new URL(`./assets/fonts/${file}`, import.meta.url)))
      } catch (error) {
        console.error(`[fonts] ${file}:`, error)
      }
    }
    // Off macOS the system UI font is a variable face, so the kit's weights land on its `wght` axis (`platform/fonts.ts`).
    installUiFont(renderer)
    startupMark('fonts-ready')
    setReady(true)
    startPerfProbe(renderer)
    startWindowShell(renderer)
    // Windows and Linux: the title bar's window state, the OS theme and motion, the saved frame (`shell/windowChrome.ts`).
    startWindowChrome(renderer)
    // One window per environment across processes; the title names the environment (`windows/`).
    startEnvironmentWindow(renderer)
    // Debug: minimize after launch to check that hidden windows stop their loops.
    if (process.env.KYBERN_DEBUG_MINIMIZE) setTimeout(() => renderer.minimizeWindow?.(), Number(process.env.KYBERN_DEBUG_MINIMIZE))
  }, [renderer])
  return ready ? children : null
}

startupMark('modules-ready')
// The notification identity (the Windows AppUserModelID) goes to the addon before any window exists, in this process and in
// every environment window's, which starts here too: the taskbar groups a window by the id it was created under.
setEarlyAppIdentity()
// KYBERN_GPUIX_SMOKE=1 (the installer check, `packaging/windows/verify-install.ps1`): prove that a packaged build
// loads its native addon with every DLL it needs and imports its whole module graph and resources, then exit before
// a window, daemon or network connection exists. A failure is written to KYBERN_GPUIX_SMOKE_OUT (a GUI build has no console).
if (process.env.KYBERN_GPUIX_SMOKE === '1') {
  const out = process.env.KYBERN_GPUIX_SMOKE_OUT
  try {
    const addon = process.env.NAPI_RS_NATIVE_LIBRARY_PATH
    if (!addon) throw Error('No patched native addon is set (NAPI_RS_NATIVE_LIBRARY_PATH)')
    createRequire(import.meta.url)(addon)
    await import('./App.tsx')
    readFileSync(new URL(`./assets/fonts/${FONTS[0]}`, import.meta.url))
    if (out) writeFileSync(out, 'ok')
  } catch (error) {
    if (out) writeFileSync(out, error instanceof Error ? error.stack ?? error.message : String(error))
    process.exit(1)
  }
  process.exit(0)
}
// Restore selection before the socket opens, so its content can hydrate while
// the workspace modules and native window initialize.
startSplitView()
connect()
// Packaged builds check the release feed 15 s after launch, then every 6 h (`platform/update.ts`).
startAppUpdateChecks()
// A line a minute in `memory.jsonl` (`perf/memoryLog.ts`), so a footprint spike can be traced afterwards.
startMemoryLog()
// React's development build adds a `performance.measure` entry for every component it renders, and Bun keeps them
// all: live turns grew a session by 40–150 MB each (`perf/memlog/README.md`). Packaged builds use production React
// (`scripts/package-app.ts`); a development run clears the entries every 10 s. Nothing in the app reads them.
if (process.env.NODE_ENV !== 'production') setInterval(() => { performance.clearMeasures(); performance.clearMarks() }, 10_000).unref()

// Resolve the endpoint and start the socket while Bun loads the workspace's
// page/editor modules. The first render still uses the complete App and fonts.
const { App } = await import('./App.tsx')
startupMark('workspace-module-ready')

// The launch backing, from the saved material of the variant the window opens in (`shell/windowMaterial.ts`).
const launchVariant: ThemeVariant = process.env.KYBERN_APPEARANCE === 'light' || process.env.KYBERN_APPEARANCE === 'dark' || appearance().theme === 'system' ? VARIANT : (appearance().theme as ThemeVariant)
const launchMaterial = appearance().window[launchVariant]
const launchBacking = backingFor(launchMaterial, resolveScope(launchMaterial, getAccessibilityDisplay()), launchVariant)

startupMark('render-start')
// Before `render`, so a throw from a timer or promise degrades to a toast instead of GPUIX's full-window error screen.
installRuntimeSafetyNet()
render(
  <ThemeProvider>
    <Fonts>
      <App />
    </Fonts>
  </ThemeProvider>,
  {
    title: 'Kybern',
    appName: 'Kybern',
    // `KYBERN_WINDOW_SIZE=WxH` lets the pair tool match a real Tauri window's size.
    width: Number(process.env.KYBERN_WINDOW_SIZE?.split('x')[0]) || 1280,
    height: Number(process.env.KYBERN_WINDOW_SIZE?.split('x')[1]) || 820,
    titlebarTransparent: true,
    // Opaque for Solid, an accessibility override or `KYBERN_OPAQUE`; else the native vibrancy (Blur Auto) or a
    // transparent window with the adjustable desktop blur behind it (`shell/windowMaterial.ts`). Windows 11: Mica or
    // Acrylic; Linux and Windows 10: opaque.
    windowBackground: nativeBackground(launchBacking),
    trafficLightX: 16,
    trafficLightY: 16,
    ...(process.env.KYBERN_PERF_PROBE === '1' ? { debugFrameOverlay: 'minimal' as const } : {}),
    minWidth: 900,
    minHeight: 600,
    // Windows and Linux reopen the saved frame here (macOS restores AppKit's frame after the window opens).
    ...(process.env.KYBERN_WINDOW_SIZE ? {} : savedFrameOptions({ width: 900, height: 600 })),
    // Linux: Kybern draws the title bar, its buttons and the resize edges (`nav/WindowControls.tsx`). X11 without a
    // compositing manager falls back to the window manager's frame.
    ...(process.platform === 'linux' && process.env.KYBERN_WINDOW_DECORATIONS !== 'server' ? { windowDecorations: 'client' } : {}),
    // An agent checking its own work must not take the keyboard from the user.
    focus: process.env.GPUIX_BACKGROUND !== '1',
    // A saved frame is restored before the window is shown (`shell/window.ts`).
    show: !hasSavedFrame(),
    // Window shortcuts (⌘N, ⌘B, ⌘K, ⌘[ ⌘], ⌘↑, ⌘,): `nav/keys.ts`.
    onKeyDown: (event) => { markUserInput(); onWindowKeyDown(event) },
    onEvent: handbackOnFocus,
  },
)
startupMark('render-returned')

// Put the window back where it was (size and position are saved as it moves).
installWindowFrame()

// GPUI's blur is colourless; give it the desktop's vibrancy material, or set the adjustable desktop blur.
installWindowMaterial(launchBacking, launchVariant)

// Automation drives the app over a stdin pipe (`@gpuix/react/automation`). If the
// driving script dies without `close()`, the pipe ends: quit instead of
// lingering as an orphaned window.
if (process.env.GPUIX_BACKGROUND === '1' && isPipe(0)) {
  process.stdin.on('end', () => process.exit(0))
  process.stdin.on('close', () => process.exit(0))
}

function isPipe(fd: number): boolean {
  try {
    // Bun and Node spawn pipes as socket pairs on macOS; a shell pipe is a FIFO.
    const stat = fstatSync(fd)
    return stat.isFIFO() || stat.isSocket()
  } catch {
    return false
  }
}
