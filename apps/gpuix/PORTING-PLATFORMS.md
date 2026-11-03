# Porting GPUIX to Intel Mac, Windows and Linux

Issue #113: retire the Tauri app (`apps/desktop`) once Kybern GPUIX runs
everywhere Tauri ships. **Done after 0.18.0**: the Tauri app was removed from
the repository without a final deprecation release (see "Tauri retirement"
at the end). This document says what is macOS-only today, how each
piece moves to Windows and Linux (or degrades gracefully), the work packages
that can run in parallel, what needs real hardware, and the ship order.
The inventory was written against `feat/gpuix-intel` (GPUIX 0.10.0, Kybern
0.10.2); the ADE-52 spike below re-checked it on GPUIX 0.11.0 by building and
running the patched addon on Linux and Windows.

| Platform | Tauri | GPUIX |
| --- | --- | --- |
| macOS Apple Silicon | `.dmg`, `.app.tar.gz` | `kybern-gpuix_<v>_aarch64.app.tar.gz` |
| macOS Intel | `.dmg`, `.app.tar.gz` | `kybern-gpuix_<v>_x86_64.app.tar.gz` |
| Windows x64 | NSIS `-setup.exe` (unsigned) | `kybern-gpuix_<v>_x64-setup.exe` (beta, unsigned per-user NSIS; ADE-78, [packaging](packaging/README.md#windows-x64-beta)) |
| Linux x64 | `.AppImage`, `.deb` | beta (ADE-78): `kybern-gpuix_<v>_x86_64.AppImage` (self-updating), `.deb`, `.rpm`, with the fallbacks below |

## ADE-52 spike: the patched addon on Linux and Windows (2026-10-22)

The patched addon (GPUIX 0.11.0 plus every Kybern patch) builds for
`x86_64-unknown-linux-gnu` and `x86_64-pc-windows-msvc` on GitHub-hosted
runners, and the real app draws on both: sidebar, a seeded thread with
Markdown, a table and a highlighted code block, and the composer. Rounded
clipping is ported to WGSL and HLSL. Scope was compile + draw, with
fallbacks; nothing below is product-ready.

How it is run: `.github/workflows/gpuix-platforms.yml` (manual) builds
`kybernd` and the addon (`native/update.sh` with `KYBERN_NATIVE_CHECKS=0`), then
`scripts/platform-smoke.ts`: a scratch daemon, one thread seeded the way
`perf/bench/seed.ts` does it (RPC for records, SQL for synthetic provider
events, no agent), and PNGs of the app and `native/clip-lab.tsx` with rounded
clipping on and off (`KYBERN_ROUNDED_CLIP=0` squares every clip chain: the
rectangular look the app had before this port). Neither runner has a GPU:

- **Linux** (`ubuntu-24.04`): Xvfb, X11 client, wgpu on Mesa **lavapipe**
  (llvmpipe Vulkan). Captured with ImageMagick `import` (wgpu has no readback
  in GPUIX yet).
- **Windows** (`windows-2022`): Win32 window, Direct3D 11 on **WARP**
  (Microsoft Basic Render Driver). Captured with the addon's
  `captureScreenshot` (GPUI renders the window's scene to an image through the
  same renderer).

Since WP7 the same workflow runs the whole check harness (`scripts/run-platform-checks.ts`, see
`native/README.md`, "Running the checks on Linux and Windows") on `ubuntu-22.04` and `windows-2022`, builds the addon through
`.github/actions/gpuix-addon`, and captures through the addon's screenshot, falling back to the X window on Linux when the
addon's screenshot is unavailable there (the smoke's `import -window root` above is that fallback, now window-sized).

`workflow_dispatch` only runs a workflow that exists on `main`. Until this
branch merges, the spike ran it from throwaway `spike/ade-52-platforms*`
branches whose single extra commit adds a push trigger for those branches;
the committed workflow stays manual-only.

Local loop (macOS host): an arm64 Ubuntu 24.04 container (OrbStack) runs the
same `cargo check` / release build of the Linux addon in about 15 s / 75 s
incrementally and the same smoke test under Xvfb + lavapipe; `naga` validates
the three WGSL variants. That loop found every Linux issue before CI did.

### Acceptance criteria

| # | Criterion | Result | Evidence |
| --- | --- | --- | --- |
| 1 | Addon builds for both targets in CI | met | Both OSes green: [39742674735](https://github.com/Tresnanda/kybern/actions/runs/39742674735) (LTO off) and [39742680467](https://github.com/Tresnanda/kybern/actions/runs/39742680467) (release profile, fat LTO). First Linux build: [39735839297](https://github.com/Tresnanda/kybern/actions/runs/39735839297) |
| 2 | App launches and renders sidebar, Markdown/code transcript, composer | met on both (Linux CI x86_64 and local arm64; Windows CI) | `ade52-linux-app.png`, `ade52-windows-app.png` |
| 3 | Rounded clipping correct in WGSL and HLSL, before/after | met on both | `ade52-linux-clip-before.png` / `-after.png`, `ade52-windows-clip-before.png` / `-after.png`; macOS `clip-check` and `rounded-check` still pass |
| 4 | Every macOS-only patch with works / fallback / blocked | met | [the table below](#macos-only-patches-off-macos) |
| 5 | Bun main thread vs GPUI UI thread, decided with evidence | met: UI thread | [Event loop decision](#event-loop-decision) |
| 6 | Re-estimated plan and ordered work packages | met | [Work packages](#work-packages) |

Screenshots are workflow artifacts (`gpuix-<os>-screens`) and copies under
`.kybern-artifacts/ade-grill/` in the main checkout; they are not committed.

### What it took

| Breaker (estimate before the spike) | What it actually needed |
| --- | --- |
| `Primitive::Backdrop` arms in wgpu and D3D renderers (0.5 d) | Two match arms that draw nothing (`platforms_gpui.py`); the material's tint quad still paints, which is the agreed fallback |
| ~40 hooks on the macOS-only `update_window` (2 d spike + 4–8 d) | **One generic bridge, no hook rewritten** (`platforms_hooks.py`): off macOS `update_window` / `update_window_without_view` box the closure into `UiCommand::Run`, run it on the `gpuix-ui` thread against the same window and wait for the result. Every existing closure was already `Send + 'static` except one `move` keyword. 12 napi methods that were stubbed "macOS-only" only because of `update_window` now share the macOS body; the save panel opens on the UI thread |
| `ContentMask` 16 → 24 bytes in WGSL/HLSL (1 d per backend) plus rounded alpha (2–3 d per backend) | Struct mirrored, the Metal chain walk ported to both languages, a clip-chain storage buffer (wgpu, group 1 binding 1) and structured buffer (D3D, `t2`, `ps_4_1`-compatible); about 1 d for both. Without it Linux drew **nothing** (every primitive read a garbage clip rect) |
| `update.sh` Mach-O only (1.5 d) | Per-target library name and output (`linux-x64-gnu`, `linux-arm64-gnu`, `win32-x64-msvc`), realign/codesign/Cocoa companion only on macOS, `.exe` test binaries, `PYTHON`/`PYTHONUTF8` for Git Bash; 0.5 d |
| (not in the inventory) | Compositor loops defaulted on everywhere: GPUI core lifted loop leaves out of the scene for Core Animation layers that only Metal creates, so spinners and shimmers would vanish off macOS. Now macOS-only (scene loops elsewhere). Upstream `getPaintedText` / `getPaintedHighlights` read the JS thread's empty thread-locals off macOS; they ask the UI thread now. `ElementBounds` lost `Copy` in a Kybern patch, which broke upstream's Linux-only `elements_at` |

All of it lived in two new drivers, `patches/platforms_gpui.py` (GPUI side,
run last by `backdrop_gpui.py`) and `patches/platforms_hooks.py` (native crate,
run last by `apply_hooks.py`). Since the fork move (WP0) they are the commits
"Linux (wgpu) and Windows (Direct3D 11) build and draw (ADE-52)" on
`Tresnanda/zed` `kybern` and "Linux and Windows: UI-thread bridge (ADE-52)" on
`Tresnanda/gpuix` `kybern`. They only edit Linux/Windows sources or add
`cfg`-gated code, so the macOS addon is unchanged apart from the gate removal
on the 12 shared methods; `KYBERN_NATIVE_CHECKS=0 ./update.sh`, the app's
typecheck and 442 unit tests, `clip-check` and `rounded-check` pass on macOS.

Addon build times on the hosted runners (cold cargo cache): Linux 4.5–6.5 min,
Windows 7.7–8.2 min, with or without fat LTO. macOS release with LTO: 6.5 min
locally. The Windows window came up at the WARP desktop's 1024×768, so its
captures are smaller.

## Event loop decision

**Decision: keep upstream's model off macOS (GPUI on its own UI thread), with
one generic bridge for Kybern's hooks. Do not pump GPUI on Bun's main thread on
Linux or Windows.**

Evidence:

1. **The cost that motivated option 2 is gone.** The old plan assumed each of
   the ~40 hooks would need a hand-written `UiCommand` variant and every future
   hook two versions. The bridge (`run_on_ui_thread`, ~60 lines) makes
   `update_window` portable with the same signature plus `Send + 'static`.
   The whole patch queue type-checked off macOS with it; one closure needed a
   `move`. Future hooks are written once; only a non-`Send` capture fails to
   compile, which is the right error.
2. **It runs.** The full app (all hooks, the Markdown document worker, the
   editor, cached islands, layout observers) runs on the UI-thread model on
   Linux, locally and in CI.
3. **Cost per call is upstream's own.** 2,000 synchronous window updates from
   JS (`native/bridge-bench.tsx`, Linux arm64 container, lavapipe): a Kybern hook through
   the bridge p50 38.5 µs, p90 44.5 µs; upstream's own synchronous
   `getWindowSize` over its channel p50 38.5 µs, p90 43.8 µs; a call that does
   not touch the window 0.3 µs. macOS (same thread) is 1.4 µs. p99 of the hook
   reached 0.8 ms when the UI thread was mid-frame on the software rasterizer.
   That is cheap for per-gesture calls, but hooks called many times per frame
   should not wait: make the fire-and-forget ones (`()` results) send without
   waiting (WP1 below), and measure calls per frame in the bench.
   **WP1 did both:** `()` hooks queue without waiting (`update_window_detached`;
   the per-frame one is the animation-frame acknowledge), and `getBridgeStats()`
   counts sends, waited calls and wait time against GPUI's served frames;
   `perf/bench` records it per window (`evidence.bridge`, served frames split
   into drawn and callbacks-only), `scripts/frame-wake-check.tsx` asserts fewer
   than 0.5 waited calls per animated frame and `platform-smoke.ts` the same
   for a scrolling transcript. The layout-observer liveness check
   (`isLayoutObserverActive` on every callback, a waited round trip per layout
   event while scrolling) is gone from `src/native/layout.ts`, and off macOS
   the transcript window (`src/transcript/Window.tsx`) takes the list anchor
   from the list's own observation instead of a waited `getListScrollTop` per
   row observation and range change: scrolling a 24-turn transcript under Xvfb
   went from 1.78 to 0.1 waited calls per frame (the wheel input excluded). The same work found that off macOS every mutation batch
   invalidated with `window.refresh()`, which turns every cached island off; it
   now takes the macOS island path (`KYBERN_FULL_INVALIDATE=1` restores the
   refresh for A/B runs).
   **Typing (0.21.5 Windows reports, fork `key-detach`):** an answer that
   streams keeps the UI thread drawing back to back (about 120 draws/s, 8 ms
   each, on a Ryzen 7 5800H), so every waited call costs about one frame. Typing
   into the composer of a 24-turn thread made 10.6 waited calls per key
   (`getInputSelection` ×4.7, `subscribeAnimationFrames`, `getFocusedElementId`,
   `getElementBounds`, `setInputDecorations`, `updateMarkdownDocument`): about
   110 ms of blocked JS per key there, and several times that on slower PCs,
   where keys queued for seconds. The intercepted key's answer
   (`resolveKeyEvent`) was one of them and threw "Timed out after 2 seconds
   waiting for a window update" into the app whenever the UI thread stayed busy
   that long. Now `resolveKeyEvent`, `subscribeAnimationFrames` and
   `setInputDecorations` are queued (malformed decorations still throw at the
   call; a stale value or range is now logged instead, so `decoration-check`'s
   `bad-range` and `stale` cases hold on macOS only), `getElementBounds` answers with the last
   painted bounds without `window.refresh()` (GPUI runs next-frame callbacks
   before it draws, so the old frame wait returned the same bounds), the
   Markdown and syntax documents go through a registry handle kept on the
   renderer (release included, so it stays in order with create), and the composer reads its
   selection once per event, its card width only with the completion menu
   open and the focused element last: 2.3 waited calls (all
   `getInputSelection`) and about 22 ms per key, key-to-paint p90 from about
   105 to 55 ms. `platform-smoke.ts` measures it and checks that a key
   answered across a 4.5 s UI-thread stall (`stallUiThread`) lands without an
   error.
4. **Option 2 has real costs that option 1 does not.** Main-thread pumping
   needs an embedded mode in `gpui_linux` (X11 and Wayland calloop dispatch
   from a Bun-polled fd) and `gpui_windows` (a `PeekMessage` pump, with
   Windows' modal size/move loop blocking JS for the whole drag), plus flipping
   roughly a hundred `cfg(target_os = "macos")` pairs in `renderer.rs` from the
   threaded branch to the embedded one. Estimated 6–9 days and a patch that
   fights upstream on every update. Not attempted: point 1 removed its reason.

Known hazard of the threaded model: a hook that reads a thread-local registry
*outside* its `update_window` closure sees the JS thread's empty copy. An audit
of every napi method found only upstream's `getPaintedText` and
`getPaintedHighlights` (fixed) and the macOS-only `idle_memory` tick pool and
window-state callback (no-ops off macOS). New hooks must keep paint-time
state reads inside the closure.

## macOS-only patches off macOS

Status: **works** (same behaviour; *verified* means seen on screen or exercised
by the smoke run), **fallback** (degrades as named), **blocked** (needs the
work package named before shipping). "UI-thread bridge" means the hook needed
nothing but `platforms_hooks.py`.

| Patch / feature | Where | Linux | Windows | Notes |
| --- | --- | --- | --- | --- |
| Rounded overflow clip (paint + hit test) | `rounded_clip*`, `shaders.metal` | works, verified | works, verified | Ported (`platforms_gpui.py`); hit-test part is in GPUI core |
| Backdrop blur / saturation | `backdrop_*`, `backdrop.metal`; WP9 `backdrop.wgsl`, `wgpu_backdrop.rs`, `backdrop.hlsl`, `directx_backdrop.rs` | works (WP9) | works (WP9) | Same kernels, crop, saturation and clip chain as Metal, over a copy of the region behind the filter. `backdrop` check: [WP9 status](#wp9-backdrop-blur-and-window-materials-status). `KYBERN_BACKDROP_FILTER=0` restores the old fallback (the fill alone) |
| Compositor loops (CA layers) | `compositor_loops_*`, `compositor_covers_gpui.py` | fallback | fallback | Scene loops at the normal frame rate (macOS-only gate added) |
| Partial-region presentation | `paint_region_*`, `paint_region.metal` | fallback | fallback | Full redraw (it is opt-in even on macOS) |
| Drawable timing, GPU clip simplification, GPU telemetry | `frame_drawable_gpui.py`, `gpu_clip*`, `gpu_telemetry_gpui.py` | fallback | fallback | Counters report 0; nothing visual |
| Memory counters, image eviction stats | `memory_counters_gpui.py`, `image_memory*` | fallback | fallback | `getMemoryStats` works through the bridge; Metal gauges and atlas bytes are 0 |
| Idle GPU release, window-state observer, autorelease tick pool | `idle_memory*` | fallback | fallback | No idle release; JS keeps its polling path |
| Large-allocation mapper, small-allocation heap | `large_alloc.rs`, `code_small*`, `code_alloc.rs` | fallback | fallback | System allocator; their stats read 0 |
| Shimmer band / hue, pop, rotate, stroke draw, hover/stream fades, subtree transforms | `shimmer_band_gpui.py`, `pop_gpui.py`, `rotate_gpui.py`, `stroke_draw*`, `hover_fade*`, `stream_fade*`, `subtree_*` | works | works | Evaluated in GPUI core's scene, not in Metal; compiled and running, not individually checked on screen |
| Frame wake | upstream `set_frame_wake_hook` (0.11); WP1 zed commit "Frame wake hook off macOS" | works (WP1, CI: idle 10 ticks/s) | works (WP1, CI: idle 10 ticks/s) | `setFrameWakeCallback` returns true off macOS, so the JS loop runs on its 100 ms fallback instead of the 8 ms poll (125 wakes/s), and wakes once when the UI thread exits. No per-frame wake: off macOS `tick()` only reads whether the UI thread runs, animations use `subscribeAnimationFrames`, and a wake per served frame cost 60–240 JS wakeups/s during agent work (the first CI run measured 62/s animating). `scripts/frame-wake-check.tsx` checks it in CI |
| Mutation invalidation (cached islands) | `request_invalidate` | works (WP1, not yet run) | works (WP1, not yet run) | Was `window.refresh()` on every batch (no cached views); now the macOS island path. `KYBERN_FULL_INVALIDATE=1` restores the refresh |
| Wayland without `wl_seat`, stale `WAYLAND_DISPLAY` | WP1 zed commit "Wayland: run without a seat, fall back to X11" | works (WP1, not yet run) | — | Headless Weston (no seat) no longer panics; a seat that appears later gets its devices. An unreachable `WAYLAND_DISPLAY` with `DISPLAY` set uses X11 |
| Per-monitor DPI | `enable_per_monitor_dpi` (0.11), `getPlatformInfo` (WP1) | — | works? | Bun's manifest declares no DPI awareness, so the UI thread's `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)` succeeds; GPUI handles `WM_DPICHANGED`. WP1 reports the mode and the window's `GetDpiForWindow`. 125/150/175 % and monitor moves need hardware |
| Variable `wght` axis | `font_weight*` (CoreText); WP8 zed commits on `wp8` | works (WP8, CI `text`) | works (WP8, CI `text`) | Every kit weight (450/550/650 too) on the face's `wght` axis. [WP8 status](#wp8-text-status) |
| Per-run text scale (`kszl`) | `text_scale_gpui.py` (CoreText); WP8 | works (WP8, CI `text`) | works (WP8, CI `text`) | `font_scale()` on both: rich-editor inline code at 0.833 |
| Fonts | `theme.ts`, `src/platform/fonts.ts` | works (WP8) | works (WP8) | `.SystemUIFont` → Segoe UI Variable Text on Windows 11, else the bundled Inter (`setSystemUiFont`); code in the bundled JetBrains Mono, whose advance the code wrap uses (`MONO_ADVANCE`). The WP7 "Menlo glyph advances" gap closes |
| Text rendering | `cosmic_text_system.rs`, `direct_write.rs` | unhinted (WP8) | unhinted (WP8) | Glyphs drawn without hinting / grid fitting, like Core Text (`KYBERN_TEXT_HINTING=1` restores it); ClearType vs grayscale stays GPUI's choice |
| Key interception, close guard, input selection/transactions, text selection, rich-editor commands and images, window background | `apply_hooks.py`, `editor9_hooks.py`, `image_memory_hooks.py`, `window_platform_hooks.py` | works (UI-thread bridge) | works (UI-thread bridge) | Were "macOS-only" stubs; the close guard is used at startup |
| Rich-editor and input key interceptors | `rich_editor.rs`, `custom_elements/input.rs` | works (WP3) | works (WP3) | No longer gated `macos`: capture, queue and replay run on the UI thread, JS answers through `resolveKeyEvent`. The editor's formatting keys are `secondary-*` (Ctrl off macOS); ⌃A/⌃E Emacs motion is macOS-only, Ctrl+Home/End are the document ends |
| Title-bar double-click action | `window_drag.rs`, `src/shell/windowChrome.ts` | works (WP3) | works (WP3) | Windows: the native caption (HTCAPTION) maximizes. Linux: GNOME's `action-double-click-titlebar` (gsettings, followed live) through `setTitlebarDoubleClick`, maximize by default. macOS keeps AppKit's preference |
| Save panel | `apply_hooks.py` `prompt_for_new_path` | works (UI thread) | works (UI thread) | Not opened in CI. `platform/files.ts` goes through `os.savePanelDialog`: the hook, else a PowerShell / zenity / kdialog panel (WP2) |
| Clipboard image | `clipboard_hooks.py` | works (bridge) | works (bridge) | Not exercised |
| Window drag regions | `window_drag.rs` | works (WP3) | works (WP3) | Windows: only `WindowControlArea::Drag` (no GPUI handler may stop the press), so drag, Aero snap, double-click and the system menu are native; controls inside are `WindowControlArea::Client` (zed fork: the topmost area wins). Linux: the move starts on the first drag after a press; a right click opens the window menu. `window-chrome` check |
| Traffic lights / window controls | `nav/TitleBar.tsx`, `nav/WindowControls.tsx`, `window_chrome.rs` | works (WP3: client-side decorations) | works (WP3) | Windows 11 caption buttons as `windowControl` hit-test areas (Snap Layouts); Linux GNOME-style buttons in the desktop's layout with resize edges, requested as client-side decorations (server-side ones, X11 without a compositing manager, draw none). [WP3 status](#wp3-window-and-input-status) |
| Window material (NSVisualEffectView) | `native/material.ts`, WP9 `platform/desktopMaterial.ts` | fallback: opaque window, the glass coats over the Solid frame colour, frosted surfaces still blur | opt-in (WP9, `KYBERN_DESKTOP_MATERIAL=dwm`) on Windows 11 22H2+: Settings › Backdrop Mica or Acrylic (DWM), tinted in Kybern's theme; by default and on Windows 10 as Linux | No adjustable blur radius off macOS. Opt-in until seen on a real Windows desktop (CI captures cannot show DWM) |
| Secure input (Carbon) | `secure_input.rs` | fallback | fallback | No-op |
| Automation test display | `automation_display.rs` | fallback | fallback | Centered on the primary display. Not needed by the WP7 harness: captures are the addon's screenshot (or the X window, Linux) and input is synthesized in-process, so the window's position never matters |
| Scroll phase injection | `scroll_hooks.py` | works (WP1) | works (WP1) | `simulateScrollPhase` on every desktop platform and in `TestGpuixRenderer`; compile-checked, on screen in `frame-wake-check`. The WP7 checks still mark every wheel step `GAPS.wheel` until a CI run passes: a pass is reported as a closed gap, then the marker goes |
| Web view (previews, Visual, Artifacts, Mermaid) | `webview.m`, `native/webview-win`, `src/native/webview.ts` | works (WP10: WebView2 under WinComp, `kybern-webview.dll`; needs the WebView2 Runtime, else WP4's fallbacks) | blocked | Linux loads no library (`webHost.available` is false; `KYBERN_GPUIX_NO_WEBVIEW=1` forces it anywhere). Fallbacks shipped in WP4: see *Without a web view*. Windows status and parity: `WEBVIEW-PLATFORMS.md` |
| Notifications | `packaging/notifications.m` (macOS); GPUI's own on Windows and Linux through `src/native/notifications.ts` | works (WP5: toasts under the installer's AUMID, `notify` check) | works (WP5: freedesktop `Notify`, `notify` check) | macOS keeps the companion. Not yet run on a real Windows toast click or a Linux desktop shell. [Details](#wp5-notifications-status) |
| objc FFI shell (frame restore, appearance, visibility, locale) | `shell/*.ts`, `native/*.ts` | works (WP3) for frame and theme | works (WP3) for frame and theme | `shell/windowChrome.ts`: the frame (size, position, maximized, display) saved through `getWindowBounds` and reopened through the window options; the OS theme live through GPUI's appearance; visibility and locale still guarded |
| `pbcopy`, `open`, `osascript`, `~/Library`, Keychain | `src/**` via `src/platform/os.ts` | fallback | fallback | WP2 done: commands per OS, unit-tested; not run on a real Windows/Linux desktop. [Details](#f-js-platform-services-src) |
| ⌘ shortcuts | `src/platform/keys.ts` + call sites | works (WP3) | works (WP3) | `isPrimary`: Ctrl off macOS (GPUIX's `cmd` is the Windows/Super key there); labels convert centrally. [WP3 status](#wp3-window-and-input-status) |
| Pointer-release watch | `preview/resize.tsx` | works | works | Guarded; the window-sized capture layer ends the drag on mouse-up or a move with no button (WP2) |
| Copy "This Mac" | environment chip, sidebar, settings | works | works | "This computer" off macOS (`osLabels()`); Finder copy becomes Explorer / Files (WP2) |

## Intel Mac (shipped in v0.10.3)

- **Addon:** one per arch, not universal. `KYBERN_GPUIX_TARGET=x86_64-apple-darwin
  native/update.sh` cross-builds `native/build/gpuix-native.darwin-x64.node` on an
  arm64 Mac. The rest of the bundle is single-arch anyway: Bun has no universal
  runtime, and `kybernd`/`kybern` are built per target. A lipo'd addon would add
  ~26 MB to every download and buy nothing. The small Objective-C companions
  (`kybern-webview.dylib`, `notifications.dylib`) are universal, built by
  `scripts/companion-flags.ts`, so the two lanes never overwrite each other's
  copies. They also now target macOS 13.0: before this, clang stamped the build
  machine's OS as the minimum (15.0 on CI, 27.0 locally), so dyld would have
  refused them on macOS 13/14.
- **Rosetta checks:** `native/tools/bun-x64.sh` compiles a stub for
  `bun-darwin-x64`; with `BUN_BE_BUN=1` that stub *is* the x64 Bun CLI. Under
  Rosetta, `update.sh` uses it for the addon load check, and for the on-screen
  checks it goes first on `PATH`.
- **Packaging:** `bun scripts/package-app.ts --arch x64` (add `--test` for local runs);
  `KYBERN_GPUIX_TARGET=x86_64-apple-darwin scripts/bundle-gpuix.sh`. The feed key is
  `darwin-x86_64-gpuix`, which `src/platform/update.ts` already derives from
  `process.arch`. `install-mac-gpuix.sh` picks the build from `uname -m`
  (`sysctl.proc_translated` keeps a Rosetta shell on the arm64 build).
- **CI:** the `gpuix` job in `desktop.yml` is a matrix over
  `aarch64-apple-darwin` and `x86_64-apple-darwin`, both on `macos-15` (arm64),
  with Intel cross-compiled. Reasons:
  - The Tauri `bundle` job already cross-builds x86_64 there.
  - Both lanes then share the same Xcode and Metal toolchain.
  - `macos-15-intel` is GitHub's last x86_64 image, with a retirement date.
  - Hosted Intel runners are slower and scarcer.
  
  Neither lane compiles the daemon: each packages the `kybernd`/`kybern` from
  dist's archive for its target (#115's single build), via `KYBERN_BIN_DIR`.
- **Tauri prompt:** `gpuix_install.rs` now offers GPUIX on Intel Macs and picks
  the feed key by arch. An x86_64 Tauri running under Rosetta installs the arm64
  GPUIX.

**Rosetta testing has an expiry date.** macOS 28 drops Rosetta for apps, so this
Apple Silicon check only works on macOS 27 or earlier. Every x86_64 launch on
macOS 27 also posts a system "App Update Required … will not open in macOS 28"
notice naming the bundle. Local Intel builds are therefore packaged with
`--test` (`Kybern GPUIX (x86_64 test).app`, `dev.kybern.gpuix.test`) and launched
as rarely as possible. Lasting Intel verification needs Intel hardware or a
hosted Intel runner (`macos-15-intel`, while it exists). The Intel product
itself is unaffected: on a real Intel Mac the app is native.

**What Rosetta cannot prove.** Rosetta translates CPU code. Metal still runs on
the Apple GPU, so nothing below is exercised:

- **Intel GPUs:** Iris/UHD integrated and AMD Radeon Pro discrete Metal drivers
  and their shader compiler.
- **Dual-GPU MacBook Pros (2016–2019):** GPU switching, plus GPUI's choice of
  the low-power device.
- **Performance on older silicon:** real AVX2 timing and memory bandwidth.
  Rosetta-under-M1 numbers are not Intel numbers.
- **Older macOS:** Intel Macs on macOS 13–15 (this Mac runs 27). The companions
  and the Bun runtime now declare 13.0, but older WebKit and AppKit behaviour
  is untested.

One pass on a real Intel Mac before the first release would cover this: launch,
transcript scroll, the backdrop blur, a preview, a Mermaid diagram, and
Activity Monitor energy.

## How upstream GPUI runs off macOS

Pinned zed (`native/upstream/zed`) splits platforms into crates:

| | macOS | Linux | Windows |
| --- | --- | --- | --- |
| Platform crate | `gpui_macos` | `gpui_linux` (Wayland + X11) | `gpui_windows` |
| Renderer | `gpui_apple` Metal, `shaders.metal` | `gpui_wgpu` (wgpu, Vulkan/GL), `shaders.wgsl`; cosmic-text | Direct3D 11 + DirectComposition, `shaders.hlsl`; DirectWrite |
| gpuix event loop | AppKit embedded in Bun's main thread (`renderer.rs` `update_window*`) | separate `gpuix-ui` thread fed by `UiCommand` over a channel; Kybern's hooks reach it through `UiCommand::Run` (`platforms_hooks.py`) | same as Linux |
| Stock npm addon | darwin-arm64 | linux-x64-gnu | win32-x64-msvc |

GPUI already ships cross-platform services that Kybern reaches for through
AppKit or shell-outs today:

- credentials: `read_credentials` / `write_credentials`
- `show_system_notification`
- `open_url`, `reveal_path`
- `prompt_for_paths`, `prompt_for_new_path`
- clipboard
- `window_appearance`
- `WindowBackgroundAppearance::MicaBackdrop` (Windows 11)

## Inventory

Effort is in focused agent-days, including the checks described, but not
review. "Fallback" is what ships if the full port is not done yet.

### A. The patched addon did not compile off macOS (done in ADE-52)

| Item | Fix | Where |
| --- | --- | --- |
| `PrimitiveBatch::Backdrop` / `Primitive::Backdrop` hit exhaustive matches in the wgpu and D3D renderers | Arms that draw nothing (fallback: the tint quad shows) | `platforms_gpui.py` |
| ~40 napi methods call the macOS-only `update_window` / `update_window_without_view` | Generic UI-thread bridge, no hook rewritten; 12 "macOS-only" stubs now share the macOS body | `platforms_hooks.py`, [decision](#event-loop-decision) |
| `ContentMask` 16 → 24 bytes, WGSL/HLSL declared `Bounds` (Linux drew nothing) | Struct mirrored; rounded chain walk ported to WGSL and HLSL | `platforms_gpui.py` |
| `update.sh` was Mach-O/BSD only | Per-target names and steps; Git Bash on Windows | `native/update.sh` |

### B. GPU features written only for Metal

Rounded clip: done on both backends (ADE-52). The other rows are post-beta
parity (WP9); their fallbacks are what the spike runs today.

| Feature | Patches | Windows (HLSL) | Linux (WGSL) | Fallback | Effort per backend |
| --- | --- | --- | --- | --- | --- |
| Rounded `overflow:hidden` clip for paint and hit tests | `rounded_clip*.{py,rs}`, `rounded_clip_alpha` in `shaders.metal` | Port the SDF alpha + clip buffer | same in WGSL | **None acceptable**: rectangular clips break every card and pill, so this ships with the first build | 2–3 |
| Backdrop blur/saturation | `backdrop_gpui.py`, `backdrop_renderer.rs`, `backdrop.metal`, `backdrop_kernel*` | Two-pass blur in a D3D11 pixel shader on a copied back buffer; Mica for the window background | wgpu compute or two render passes | Opaque tinted fill (the existing reduced-transparency look) | 3–4 |
| Shimmer band / hue animations | `shimmer_band_gpui.py` | HLSL | WGSL | Static colour, as with Reduce Motion | 1 |
| Compositor loops (breathe/shimmer/rotate handed to Core Animation) | `compositor_loops_*` | DirectComposition animations later | none | `KYBERN_COMPOSITOR_LOOPS=0` scene path, already checked by `perf/motion-fps/lab-check.ts` | 0 (fallback), 4 for DComp |
| Partial-region presentation | `paint_region_*`, `paint_region.metal` | `Present1` dirty rects | wgpu has no partial present | Full-frame present (higher idle-animation cost; measure) | 0 / 3 |
| GPU clip simplification, drawable timing, telemetry, idle GPU release, image eviction | `gpu_clip*`, `frame_drawable_gpui.py`, `gpu_telemetry_gpui.py`, `memory_counters_gpui.py`, `image_memory_gpui.py`, `idle_memory_gpui.py` | Atlas counters from the DX atlas | wgpu atlas | Counters report 0; release on idle skipped | 2 |
| Frame waker + presentation timestamps (CVDisplayLink) | `frame_gpui.py`, `frame_clock.rs` | Waker done in WP1 (GPUI's DWM vsync thread); presentation timestamps (DXGI frame statistics) not done | Waker done in WP1 (Wayland frame callbacks, X11 refresh loop); X11 Present timestamps not done | Presentation timestamps report nothing | 1 (timestamps) |

### C. Text

| Feature | Where | Windows | Linux | Effort |
| --- | --- | --- | --- | --- |
| Variable `wght` axis | `font_weight_gpui.py`, `font_weight.rs` (CoreText) | done (WP8): DirectWrite font axis values | done (WP8): cosmic-text weight + swash coordinates | 2 each |
| Per-run text scale (`kszl`) | `text_scale_gpui.py` | done (WP8) | done (WP8) | 1 each |
| `register_font` for the bundled fonts | `apply_hooks.py:272-279` (macOS-only branch) | `text_system().add_fonts` is cross-platform: drop the gate | same | 0.5 |
| System fonts | `theme.ts:141-142` (`.SystemUIFont`, Menlo) | done (WP8): Segoe UI Variable Text on Windows 11, else bundled Inter; JetBrains Mono | done (WP8): bundled Inter; JetBrains Mono | 1 |

### WP8 text: status

Fork branches `wp8` (on `next`, merged with its ci-fix heads; `native/FORK` pins them until they are merged into
`kybern`):

| Fork | Commit | What |
| --- | --- | --- |
| zed | "Variable font weights and kszl on DirectWrite and cosmic-text (WP8)" | **cosmic-text** (Linux): it already shapes a span at its `Attrs` weight on a `wght` axis, but GPUI passed the face's weight and measured and drew the default instance. A font now carries its weight and normalized coordinates into the attributes, metrics, advances and the swash scaler; `kszl` runs are shaped with scaled metrics; fallback glyphs keep the run's weight and scale. **DirectWrite** (Windows): a variable face is re-created from its `IDWriteFontResource` at the requested `wght` (other axes kept), its layout ranges get the same axis values (`IDWriteTextLayout4::SetFontAxisValues`), and glyph runs map back by family, style and `wght`; a `kszl` font is its unscaled font with a scale, told apart by em size. `KYBERN_FONT_WEIGHT_AXIS=0` keeps the nearest face, as on macOS |
| zed | "Choose the family .SystemUIFont resolves to (WP8)" | `TextSystem::set_system_ui_font`: GPUI core resolves `.SystemUIFont` (and text with no family) to the app's choice before the platform text system sees it |
| zed | "DirectWrite: axis values on every variable range, KYBERN_FONT_DEBUG", "find variable fonts by typographic family; unhinted glyphs" | The font set lists a variable font's named instances under their weight-stretch-style family, so `Inter Variable` matched nothing (every weight drew one fallback face): a family with no match is looked up as a typographic family and matched and laid out under its WWS name. `KYBERN_FONT_DEBUG=1` prints the matching (the `text` check collects it). No grid fitting |
| gpuix | "setSystemUiFont, and use the zed wp8 branch" | napi `setSystemUiFont(family \| null)` (UI thread off macOS, like `registerFont`) |
| zed | "WP8 review fixes: kszl per input run, system UI fallback, VS16 emoji, hinting at 1x" (`wp8fix`) | **DirectWrite**: a glyph run takes its `kszl` scale from the input run it starts in (`textPosition`) and a scaled font for the face it resolved to, made on demand per (font, scale ‰): fonts that share a face (the rich editor's `tnum` list numbers and its task chips) or a fallback face (emoji in inline code) drew at full size. Cache entries hold their face. Emoji presentation (U+FE0F, keycaps) goes to Segoe UI Emoji; **cosmic-text** spans them to Noto Color Emoji. **Core**: an unresolvable `setSystemUiFont` family falls back to the platform UI font at the same weight (unit test). Grid fitting / swash hinting stay on below 1.5x |

Kybern side: `src/platform/fonts.ts` picks the faces (`fontPlan`, unit-tested) and `installUiFont` registers Inter
and calls `setSystemUiFont` at startup (`main.tsx`) and in labs (`registerBrandFonts`); `theme.ts` exports `FONT_MONO`
(Menlo / JetBrains Mono) and `MONO_ADVANCE` (0.602 / 0.6 em), which `codeWidth.ts`, the diff marker and the rounded
check use. macOS is unchanged: SF Pro, Menlo, CoreText untouched (`setSystemUiFont` is never called there).

The `text` check (`native/text-check.ts` on `text-lab.tsx`) asserts: every weight of the UI font and of Inter paints
wider than the one below it; line boxes are the kit line heights; Inter's and JetBrains Mono's cap heights and
baselines are within 1 px of macOS (`text-macos.json`, `--record` on a Mac); the code face paints at `MONO_ADVANCE`;
inline code is drawn at 0.833 (`kszl`); emoji are in colour; CJK falls back to a full-em font (the Linux runners
install Noto Color Emoji and Noto CJK). It writes `text-<os>.png` and `text-metrics-<os>.json`. Bounds are whole
pixels, so the specimen uses runs long enough for one weight step to move them.

Verified 2026-10-26: `text` and `rounded` pass `--strict` on Linux X11 (lavapipe) and Windows Server 2022 (WARP)
(`gpuix-platforms.yml` run 40269440313), and every other check passes on Linux X11, Wayland and Windows with this
pin (run 40267454901; its Windows `text` failure was the previous, whole-row cap measure). On the user's Windows 11
PC (RTX 3060) `text` passes with `.SystemUIFont` on Segoe UI Variable Text (300→700: 506, 532, 537, 542, 546, 550, 561,
572 px). Inter's ladder is within 1 px of macOS on Linux and 2–3 px wider on DirectWrite; cap heights and baselines are
within 0.8 px of macOS everywhere. The WP7 "Menlo glyph advances" gap passes in both `rounded` steps. Known gap: Windows has
no flag emoji (Segoe UI Emoji draws 🇯🇵 as "JP"). The ❤️ gap is closed by the review fixes below.

WP8 review fixes (`wp8fix`): the `text` check adds a task chip shaped after a `tnum` list on the same face (as wide as
one shaped before it), an emoji inside inline code (drawn at 0.833 of one outside it) and a U+FE0F row (❤️ ⚠️ ☀️ 1️⃣ in
colour). Off macOS `installUiFont` registers Inter only when the plan draws with it (not with Segoe UI Variable) plus
JetBrains Mono SemiBold and Bold (v2.304, for code at 600 and bold syntax; macOS keeps its faces). Grid fitting returns
at 1x and 1.25x; hinted cap heights and baselines snap to whole pixels (up to 1.33 px from macOS on the Linux runner), so
the cap check allows 1.5 px there.

### D. Window and input integration

| Feature | Where | Windows | Linux | Fallback | Effort |
| --- | --- | --- | --- | --- | --- |
| Traffic lights, title-bar drag and double-click | `window_drag.rs`, `nav/TitleBar.tsx`, `window_platform_hooks.py` | Custom caption buttons with `WindowControlArea` (Min/Max/Close) plus snap layouts | CSD on Wayland (`start_window_move` exists), server decorations on X11 | Native title bar | 2 + 2 |
| Window material (NSVisualEffectView, adjustable blur) | `native/material.ts`, `native/windowBlur.ts`, `shell/windowMaterial.ts`, `platform/translucency.ts`, `platform/material.ts` | Mica (Windows 11), opaque on 10; no equivalent of the adjustable blur: Solid/Auto only | Opaque; no equivalent of the adjustable blur: Solid/Auto only | Opaque | 1 |
| Close guard, key intercept | `close_guard.rs`, `apply_hooks.py` | `WM_CLOSE` / GPUI `on_should_close` | GPUI `on_should_close` | — | 1 each |
| Secure input | `secure_input.rs` (Carbon) | No equivalent; no-op | No-op | Already no-op | 0 |
| Automation test display | `automation_display.rs` (CoreGraphics) | Offscreen / second desktop | Xvfb or a headless Wayland compositor | — | 1 |
| ⌘ shortcuts | 26 files test `modifiers.cmd`; gpuix maps `cmd` to `platform`, which is the Windows/Super key off macOS | One `isPrimary(mods)` helper (Ctrl off macOS), with conflict review for the terminal (Ctrl+C) and editors | same | — | 2 |
| Window frame, appearance, visibility (objc FFI) | `shell/window.ts`, `shell/appearance.ts`, `native/visibility.ts`, `native/windowState.ts`, `locale.ts` | GPUI `window_appearance`; restore through GPUI window bounds; `Intl` locale | same | Already guarded: no system theme, no restore | 2 |
| Pointer-release watch | `split/Workspace.tsx:45`, `tasks/drag.tsx:77` guarded; **`preview/resize.tsx:16` unguarded, throws** | GPUI mouse-up capture | same | Guard `resize.tsx` (bug) | 0.5 |
| Reduce motion, transparency, contrast | `platform/material.ts`, `platform/reducedMotion.ts`; WP9 review `platform/osDisplay.ts` (transparency, contrast) | Transparency effects (`EnableTransparency`) and contrast themes (`SPI_GETHIGHCONTRAST`) through bun:ffi, done; reduced motion: WP3 | GNOME `high-contrast` (live, `gsettings monitor`) and the portal's `contrast` (launch), done; reduced motion: WP3 | Defaults | 1 each |

### E. Companion libraries

**Web view** (`native/webview.m`, 287 lines; `src/native/webview.ts`).
- **What it backs:** Preview, Visual, Artifacts and the mini-player, plus
  Mermaid (a hidden document returning an SVG that GPUI/resvg draws).
- **API:** a JSON command channel (`create` / `place` / `present` / `occlusion` /
  `navigate` / `post` / …) plus events.
- **Native behaviour it needs:** rounded clips, CA opacity/scale animation,
  occlusion masks, a custom `tauri://localhost/embedded/` scheme, an isolated
  content world, and Cmd-shortcut re-emission.

Ports:
- **Windows (8–12 days):** WebView2 `CompositionController` hosted in GPUI's
  DirectComposition tree. A child HWND cannot do the rounded clips, animation or
  occlusion. The rest maps directly:
  - `WebResourceRequested` for the scheme
  - `CoreWebView2Frame` script for the isolated world
  - `AcceleratorKeyPressed` for shortcuts
  - an InPrivate profile
  
  The same JSON channel stays, so `webview.ts` changes only its loader. Ship the
  Evergreen bootstrapper in the installer for Windows 10 machines without the
  runtime.
- **Linux:** GPUI is not GTK, and there is no X11/Wayland embedding that
  survives GTK4 or Wayland.
  - Phase 1 (3 days): a WebKitGTK helper *process* (its own GTK main loop, JSON
    over stdio) that renders Mermaid headlessly. Previews open in the
    browser (`xdg-open`) through the existing relay.
  - Phase 2 (10–15 days, risky): offscreen WPE WebKit into a wgpu texture with
    forwarded input.
- **Both platforms, interim:** the helper-process approach also gives Windows
  Mermaid before the composition host is done.

**Without a web view (WP4, shipped).** `webHost.available` (from `src/native/webview-support.ts`) gates every
surface; `webHost` calls are no-ops and `frame`/`snapshotRect` reject. Each surface checks the flag, never catches.
- **Previews** (dock tab, `previews.open`): the page opens in the default browser when ready (`store.ts`; a server on
  another machine opens through the local relay URL) and the tab says "Opened in your browser" with Open again and
  Copy link. The entry keeps its ticket and relay until it is replaced, closed or the environment resets. No mini player,
  device toolbar or native documents.
- **Visuals:** the daemon picture stays. A click does not go live; the caption has Open in browser (a fresh
  `threads.visuals.frame` ticket, served once, 60 s). Full screen and visual tabs show the picture at size
  (`VisualStill.tsx`). No Open in a tab.
- **Questions (`await_answer`):** the browser cannot answer. The ticket page is a top-level document, and
  `visuals/bootstrap.js` posts `kybern.answer.*` only when `window.parent !== window`; its CSP (`sandbox allow-scripts`,
  `connect-src https:`) also keeps it from the daemon. So the bar offers Reply in your own words (focuses the composer;
  the existing `replied` path) and Skip. A page-side answer needs the WP10 web view, or a daemon endpoint that
  accepts an answer for a frame ticket.
- **Artifacts:** the frame becomes a state with Open in browser (`threads.artifacts.preview` ticket, single use);
  Source still works.
- **Mermaid:** the fence keeps its source under a "Mermaid diagram" label with a note. The daemon's headless renderer
  (`threads.visuals.preview`) cannot take the 5 MB Mermaid bundle (512 KB page cap), so no picture yet. A WebKitGTK
  helper (phase 1 above) or WP10 draws it.
- Check: `bun scripts/nowebview-app-check.ts` (see its header; pass `macos` to prove the embedded path is unchanged).

**Notifications** (`packaging/notifications.m`;
`src/native/notifications.ts`: `status` / `requestPermission` / `post` /
`onClick`). Replace the `.m` with addon napi methods over GPUI's
`show_system_notification` and `on_system_notification_response`.
- Linux: D-Bus `org.freedesktop.Notifications`.
- Windows: toast notifications. These need an AppUserModelID registered by a
  Start-menu shortcut that the installer creates.
- Effort: 2–3 days for both.

### F. JS platform services (`src/`)

Done in WP2: every call site goes through `src/platform/os.ts` (Windows and Linux are unit-tested with simulated
platforms only; nothing here has run on a real desktop yet). macOS runs the same commands as before. Off macOS the
addon's binding is used when `setOsHost(renderer)` finds it, otherwise the command in the table. Status: **fallback** =
command-based, works once the tool is present, with a sentence naming the tool when it is not.

| Service | Files | Windows | Linux |
| --- | --- | --- | --- |
| Clipboard text (`pbcopy` / `pbpaste`) | `transcript/store.ts`, `notes/NoteMenu.tsx`, `composer/TokenTextarea.tsx`, `terminal/Terminal.tsx`, `pulls/PrStates.tsx` | fallback: PowerShell `Set-Clipboard` / `Get-Clipboard` (UTF-8 on stdin) | fallback: `wl-copy`/`wl-paste` on Wayland, `xclip` then `xsel` on X11 (and the other family as a last try); "needs wl-clipboard / xclip or xsel" when none |
| Clipboard image | `platform/files.ts`, `composer/clipboard.ts` | works: addon `writeClipboardImage`, `readClipboardImage`, `readClipboardPaths` | same |
| Open URL or file (`open`) | `settings/external.ts`, `preview/browser.ts`, `transcript/markdown.tsx`, `dock/Diff.tsx`, `notes/NoteEditor.tsx`, `pulls/PrStates.tsx`, `dialogs/Repositories.tsx`, `draft/InstructionsOffer.tsx`, `transcript/OutsideChange.tsx`, `environment/Panel.tsx` | fallback: `rundll32 url.dll,FileProtocolHandler` (never cmd.exe, so `&` is safe) | fallback: `xdg-open`, then `gio open` |
| Reveal (`open -R`): "Reveal in Finder" / "Show in Explorer" / "Show in Files" | `dock/Explorer.tsx`, `dialogs/Repositories.tsx`, `environment/Panel.tsx`, `transcript/ThreadImage.tsx`, `settings/external.ts` | fallback: `explorer.exe /select,<path>` | fallback: D-Bus `org.freedesktop.FileManager1.ShowItems` (`dbus-send`), then `xdg-open` / `gio open` of the parent folder |
| Folder picker (`osascript`) | `settings/external.ts` `pickFolder` | works if the addon exposes `promptForPaths` (GPUIX ships it); else PowerShell `FolderBrowserDialog` | same; else `zenity`, then `kdialog` |
| Admin prompt (`osascript … with administrator privileges`) | `platform/cli.ts` | none: per-user install | none: `~/.local/bin` only; a read-only folder says how to fix it |
| Save panel | `platform/files.ts` | works: addon `promptForNewPath`; else PowerShell `SaveFileDialog` | works: addon; else `zenity --save` / `kdialog` |
| Config and cache folders | `settings/appearance.ts`, `platform/prefs.ts`, `environments/registry.ts` (shared with Tauri: `dev.kybern.desktop`), `nav/live.ts`, `composer/store.ts`, `transcript/imageFiles.ts`, `perf/memoryLog.ts` | `%APPDATA%\kybern-gpuix`, `%LOCALAPPDATA%\kybern-gpuix` | `$XDG_CONFIG_HOME` (`~/.config`), `$XDG_CACHE_HOME` (`~/.cache`) |
| Credentials | `environments/credentials.ts` | The bundled `kybern credentials get\|set\|delete` command (`crates/kybern-cli/src/credentials.rs`): the same `keyring` crate and entries as the Tauri client, secret on stdin/stdout, async, cached per id. Exit 3 missing, 4 locked or dismissed (an error: "Unlock your keyring"), 5 no store. Fallback 0600 file in `%LOCALAPPDATA%\kybern-gpuix\credentials` only for exit 5 | The same command over the Secret Service (libdbus compiled in for the static musl build); with no session bus or Secret Service (exit 5) the 0600 file store in `~/.config/kybern-gpuix/credentials` (warned once, no secret logged); a locked keyring never falls back to the file |
| Login-shell PATH, Homebrew | `shell/daemon.ts`, `platform/cli.ts` (`pathPlan`) | inherited PATH, `;`, `.exe` names (`bundledCommand`, `resolveCommand`) | `$SHELL -ilc`; `/home/linuxbrew/.linuxbrew/bin` instead of `/opt/homebrew/bin` |
| CLI install | `platform/cli.ts`, `settings/About.tsx` | the installer owns PATH: the row reports whether `kybern` is there, no Install button | AppImage: `~/.local/bin/kybern` symlink to the AppImage file (needs AppRun to run the bundled `kybern` when `$ARGV0` is `kybern`; WP6 lane); deb/rpm: the package owns `/usr/bin/kybern`, reported only; portable build: `~/.local/bin` symlink |
| `screencapture -l` | `transcript/live.ts` via `os.captureWindowImage` | test-only: the automation client's `screenshot`; scripts say so and stop | same |
| Window capture attach, `sips` downscale, reduced motion and appearance (`defaults`), screen-recording check | `requests/attach.ts`, `transcript/imageFiles.ts`, `platform/reducedMotion.ts`, `ui.tsx`, `pick/screenRecording.ts` | the attach item is hidden; images show at full size; appearance from `AppsUseLightTheme` then GPUI live, reduced motion from `SPI_GETCLIENTAREAANIMATION` (WP3); no screen-recording card | GNOME `color-scheme` then the portal live, `enable-animations` (gsettings, monitored) (WP3) |
| Editors ("Open in…") | `dock/editors.ts` | "Default app" (the system's choice) | `code`, `cursor`, `windsurf`, `zed`, `subl`, `idea`, `webstorm` on PATH, plus "Default app" |

#### Native bindings wanted (not added: `native/` is frozen for the fork move)

`os.ts` takes these from the renderer when present (off macOS only, so macOS keeps its commands). Each is a napi method
over a GPUI `App` call, run on the UI thread through `run_on_ui_thread`:

| napi method | Signature | GPUI API | Replaces |
| --- | --- | --- | --- |
| `writeClipboardText` | `(text: string) => void` | `cx.write_to_clipboard(ClipboardItem::new_string)` | `pbcopy`, PowerShell, `wl-copy`/`xclip` |
| `readClipboardText` | `() => string \| null` | `cx.read_from_clipboard()` → text entry | `pbpaste`, PowerShell, `wl-paste`/`xclip` |
| `openUrl` | `(url: string) => void` | `cx.open_url` | `rundll32`, `xdg-open` |
| `openPath` | `(path: string) => void` | `cx.open_with_system` | same, for files and folders |
| `revealPath` | `(path: string) => void` | `cx.reveal_path` | `explorer.exe /select,`, FileManager1 |
| `readCredential` / `writeCredential` / `deleteCredential` | `(service: string, account: string[, secret: string]) => string \| null` | `cx.read_credentials` / `write_credentials` / `delete_credentials` (Credential Manager, Secret Service) | PowerShell P/Invoke, `secret-tool`; wire them into `credentials.ts` as a store between the platform store and the file |
| `captureWindowImage` (optional) | `(path: string) => void` | already `captureScreenshot` | nothing new |

`promptForPaths`, `promptForNewPath`, the clipboard-image methods and `captureScreenshot` already exist.

Left on macOS only, deliberately: `requests/Permission.tsx` and `Pick.tsx` System Settings flows (the daemon only sends them
on macOS), `platform/update.ts` (`/bin/mkdir`, `/usr/bin/tar`: WP6), the remote SSH install scripts (they run on the remote),
`locale.ts`, `shell/window.ts` and the other objc FFI shell (WP3), and every `screencapture`, `pbcopy`, `security`, `ps` or
`pgrep` call in the `*.verify.ts`, `verify-*.ts`, `*-check.ts`, `perf/**` and `scripts/**` test scripts, which also use Swift
helpers and NSEvent synthesis (WP7).

### G. Packaging, updates, CI

**Linux status (ADE-78, WP0 + WP6(L)):** the Linux column is built.
`.github/actions/gpuix-addon` takes `x86_64-unknown-linux-gnu` (the cache key
hashes rustc, cc, ld, glibc and the OS release instead of Xcode), and
`gpuix-addon.yml` warms that cache on `ubuntu-22.04`. The `gpuix-linux` job of
`desktop.yml` (its own job on `ubuntu-22.04`, so that like `gpuix-windows` a
failure leaves only its manifest entry out) packages dist's static musl
`kybernd`/`kybern` with `scripts/bundle-gpuix-linux.sh` →
`scripts/package-linux.ts`: an AppImage (appimagetool 1.9.1 and type2-runtime
pinned by checksum), a `.deb` and an `.rpm`, the AppImage signed as
`linux-x86_64-gpuix`. The updater replaces a running AppImage in place; deb and
rpm installs announce the release with the package link and install command;
`scripts/install-linux-gpuix.sh` installs the distribution's package or the
AppImage (and `--uninstall`s it). The floor is glibc 2.35 (ubuntu-22.04), so
RHEL, Rocky and Alma 9, Debian 11 and Ubuntu 20.04 are unsupported for now; the
installer checks first and says so. The app is Bun's baseline x64 build (no AVX2
needed). The AppImage updater accepts only the signed file
`kybern-gpuix_<newer version>_x86_64.AppImage` whose bytes are an x86-64 type 2
AppImage; a daemon is restarted only for a newer bundled version
(`shell/daemon.ts` `daemonPlan`), and the store refuses a schema a newer build
migrated. Layout, dependencies and the side-by-side rule with the Tauri package
are in `packaging/README.md`. Not proven until CI and real machines run it: the
lane itself, the release AppImage's self-update and relaunch, and every item
under [What needs real hardware or CI](#what-needs-real-hardware-or-ci).

| Item | Windows | Linux | Effort |
| --- | --- | --- | --- |
| App | `bun build --compile --target=bun-windows-x64`; `Kybern GPUIX.exe` + `gpuix-native.win32-x64-msvc.node` + `kybernd.exe` + `kybern.exe` + `kybern-webview.dll` | `bun-linux-x64-baseline` (glibc; no AVX2 required); `.node` + daemon + CLI | 2 + 2 |
| Installer | NSIS per-user into `%LOCALAPPDATA%\Programs\Kybern GPUIX`, Start-menu shortcut carrying the AUMID (**done**, ADE-78; WebView2 bootstrapper only when WP10 needs it). Unsigned like Tauri today (SmartScreen warning) until a certificate exists | AppImage (appimagetool, built on ubuntu-22.04 for the same glibc floor as Tauri) + `.deb` with a `.desktop` file and icon | 3 + 3 |
| Updater (`src/platform/update.ts`) | Keys `windows-x86_64-gpuix`; download the signed installer, run it silently `/S`, relaunch | `linux-x86_64-gpuix`; replace the AppImage in place (`$APPIMAGE`) and relaunch; `.deb` installs only announce | 2 + 2 |
| `updater-manifest.mjs`, `bundle-gpuix.*` | New asset names and keys | same | 1 |
| CI | `windows-2022` lane: build the addon from source (no display checks), package, sign the archive with the updater key | `ubuntu-22.04` lane: same, plus `libxkbcommon`/`wayland`/`vulkan` dev packages | 2 + 2 |
| Parity checks | **First set done (WP7):** `native/check-harness.ts` + `scripts/run-platform-checks.ts` run `clip`, `rounded`, `markdown-document`, `focus`, the platform smoke and the WP4 no-web-view check through the addon's automation API on macOS, Xvfb + lavapipe and WARP (`gpuix-platforms.yml`). The rest of the `*-check.ts` / `perf/**` scripts still use `screencapture`, Swift and NSEvent | Xvfb done; headless Weston runs `frame-wake` and the smoke without captures (WP1's `wl_seat` fix; on by default, the `wayland` input turns it off) | 5–8 |

### Status: Windows packaging and CI (ADE-78, WP0(W) + WP6(W))

Landed in code, **not yet exercised on a real release or real hardware**:

- `.github/actions/gpuix-addon` takes `x86_64-pc-windows-msvc` (library path and tool hash per
  target; macOS unchanged).
- `scripts/package-windows.ts` (dispatched from `package-app.ts`) compiles the app with
  `bun-windows-x64` and builds `packaging/windows/installer.nsi`: per-user install in
  `%LOCALAPPDATA%\Programs\Kybern GPUIX`, Start-menu shortcut with the AppUserModelID
  `dev.kybern.gpuix`, Add/Remove Programs, silent `/S` and `/UPDATE`, the kybern command on the
  user Path (only `bin`, appended, so the Tauri app's wins), app-local C++ runtime. No WebView2:
  nothing here needs it until WP10. Uninstall stops the daemon running from the folder first.
- `desktop.yml` `gpuix-windows` job (its own job while it is a beta, so a failure cannot skip the
  manifest): packages, signs with the updater key, then installs, smoke-runs, updates (over a
  running daemon) and uninstalls the result on the runner (`verify-install.ps1`, blocking) before
  uploading `kybern-gpuix_<v>_x64-setup.exe` + `.sig`; `updater-manifest.mjs` publishes them as
  `windows-x86_64-gpuix` when they exist.
- `src/platform/update.ts`: Windows downloads and verifies the installer (its signature must name
  this release's file, and the version must be newer), asks the other window processes to quit,
  runs the installer silently with `/D=` the running copy's folder, then quits; the installer
  relaunches. Child processes of the Rust daemon and drivers start without console windows
  (`kybern_protocol::process::NoWindow`, `CREATE_NO_WINDOW`; the terminal PTY is left alone) and
  the TypeScript spawns of console programs pass `windowsHide`.

Still open for Windows: the WP1 CI run and a DPI pass on real hardware (the code is in, see
[WP1 status](#wp1-native-platform-core-status)), the Windows half of WP3 (caption buttons, key helper), WP2
(credentials, CLI install in Settings, shell-outs), WP4 (previews in the browser), WP5 (toasts use
the AppUserModelID registered here), Authenticode signing, and the real-hardware list below.

## Work packages

Re-estimated on 2026-10-22 from the spike. Effort is focused agent-days
including checks, not review. The previous plan ("9–12 agent-weeks, 5–7
calendar weeks with four lanes") summed its own table to roughly 70–95 days;
about 20 of those (the old WP0 and the rounded-clip half of WP1) are now done
in about 2 days, and the compile risk that sized the rest is gone. What is
left is ordinary platform work, mostly in TypeScript, plus packaging.

Ordered by when to start; the "Lane" column is a suggested split for parallel
agents (one OS-specific lane can split W/L).

| WP | Scope | Owns | Depends on | Days | Lane |
| --- | --- | --- | --- | --- | --- |
| WP0 | Forks + CI. ~~Move to `Tresnanda/gpuix` and `Tresnanda/zed` with the patch queue as commits~~ (done 2026-10-25, see [Where the forks help](#where-the-forks-help)). Then: `desktop.yml` lanes that build and cache the Linux/Windows addons (generalize `.github/actions/gpuix-addon` by target), run the app's unit tests there, and keep `gpuix-platforms.yml` as the on-screen check. **Linux and Windows addon lanes and caches done (ADE-78)** | `native/update.sh`, `.github/actions/gpuix-addon`, `.github/workflows/*gpuix*` | — | 2–3 (+ fork move) | A |
| WP1 (code done, CI run pending) | Native platform core: confirm Windows on screen at DPI 100/150 %; Wayland (a real GNOME/KDE session: headless Weston has no `wl_seat` and upstream `gpui_linux` unwraps it, `wayland/client.rs:832`); bridge cost: `()`-returning hooks send without waiting, count bridged calls per frame in `perf/bench`; unify scroll-phase injection; a frame-wake hook on Linux/Windows instead of the timer (Wayland frame callback / DXGI `WaitForVBlank`). [Status](#wp1-native-platform-core-status) | fork commits "… (WP1)" on both `wp1` branches, `perf/bench`, `scripts/frame-wake-check.tsx` | WP0 | 3–5 | A |
| WP2 (done) | JS platform services (section F): `src/platform/os.ts` over GPUI clipboard, `open_url`/`reveal_path`, path prompts, credentials (Credential Manager / Secret Service, file store fallback), config/cache dirs, PATH and `.exe`, CLI install, "This computer" copy, the `preview/resize.tsx` guard | `src/platform/**`, `src/environments/credentials.ts`, shell-out call sites | — (JS only; mock the addon) | 5–7 | B |
| WP3 (code done, CI below) | Window and input (section D): caption buttons and drag/snap on Windows (`WindowControlArea`), CSD buttons on Linux, title-bar double-click in JS, one `isPrimary(mods)` key helper with a conflict review (terminal Ctrl+C, editors), the rich-editor key interceptor off macOS, close guard and DPI checks | `src/nav/TitleBar.tsx`, key helper + call sites, fork: `packages/native/src/{window_*,rich_editor}.rs` | WP1 | 6–8 | C |
| WP4 | Previews and Mermaid without the embedded web view: open previews in the default browser through the existing relay, Mermaid as static text (or SVG from a headless helper on Linux later) | `src/native/webview.ts` loader, `src/preview/browser.ts`, `src/transcript/mermaid.ts` | WP2 | 2–3 | B |
| WP5 (done; CI-verified, real-PC click pending: see below) | Notifications over GPUI (`show_system_notification`); Windows needs the AUMID from WP7's installer | new fork commits (native crate), `src/native/notifications.ts` | WP1 | 2–3 | C |
| WP6 (L/W) | Packaging and updater (section G): `bun build --compile` per target, AppImage + `.deb` (L), NSIS per-user installer with the AUMID shortcut (W), updater keys and in-place update, manifest entries. **L and W in (ADE-78): AppImage, `.deb` and `.rpm`; NSIS installer; not yet released** | `scripts/package-app.ts`, `scripts/bundle-gpuix*`, `scripts/updater-manifest.mjs`, `src/platform/update.ts` | WP0 | L 5–6, W 6–8 | D |
| WP7 (first set done, CI-unverified) | Cross-platform check harness: `native/check-harness.ts` (capture per OS, tolerance, known-gap steps), `scripts/run-platform-checks.ts` (runner, summary, artifacts), ported `clip-check`, `rounded-check`, `markdown-document-check`, `focus-check`, `platform-smoke.ts` (now asserting sidebar, Markdown/code transcript, composer) and `nowebview-app-check.ts`; `gpuix-platforms.yml` runs them on `ubuntu-22.04` (Xvfb + lavapipe), `windows-2022` (WARP) and headless Weston (`frame-wake` + smoke, no captures). WP1's `scripts/frame-wake-check.tsx` is the `frame-wake` check id. Passing on macOS; the Linux/Windows runs have not happened yet. Left: port the other checks, flip `--strict` once WP1/WP3 close the gaps | `native/check-harness.ts`, `native/*-check.ts`, `scripts/{run-platform-checks,platform-smoke,nowebview-app-check}.ts`, `.github/workflows/gpuix-platforms.yml` | WP1 | 4–6 | A |
| — | **Linux beta** = WP0–WP5 + WP6(L) + WP7 | | | ≈ 30–41 | 3–4 lanes: ≈ 2.5–3 calendar weeks |
| — | **Windows beta** adds WP6(W) and Windows-only fixes from WP1/WP3 | | | ≈ +6–10 | ≈ +1–1.5 weeks |
| WP8 (code done, see [status](#wp8-text-status)) | Text parity (section C): variable `wght` and `kszl` on DirectWrite and cosmic-text, system font choice | zed fork: `gpui_macos/src/font_weight.rs`, the "Per-run text size scale (kszl)" commit, `src/theme.ts` | WP0 | 4–6 | after beta |
| WP9 (code done) | GPU parity: backdrop blur in WGSL and HLSL (scratch copy + two-pass blur), Mica/Acrylic on Windows 11. [Status](#wp9-backdrop-blur-and-window-materials-status) | fork `wp9` commits (`gpui_wgpu`, `gpui_windows`, `gpui::backdrop_kernel`), `native/backdrop-check.ts`, `src/platform/desktopMaterial.ts` | WP0 | 6–8 | after beta |
| WP10 (W done, L in) | Embedded web view: WebView2 composition host (W, done: `native/webview-win`, fork `wp10-win` commits, CI check `webview`; see `WEBVIEW-PLATFORMS.md`); WPE WebKit (L, in 2026-10-26: forks' `wp10-linux`, WPE 2.54.2 built and bundled from ubuntu-22.04, drawn by GPUIX's `<websurface>` as a texture; status, sandbox findings and gaps in [WEBVIEW-PLATFORMS.md](WEBVIEW-PLATFORMS.md#linux-implementation-wp10-2026-10-26); security review fixes: WebKit never starts an unsandboxed web process (`wpe-sandbox.patch`), no downloads or page dialogs, wrapper only as the top frame. **Rebuild schedule:** the bundled jammy libraries and `bwrap` get no Ubuntu security updates on users' machines, so re-run `bundle-wpe.sh` for every release and at least monthly, and release when a USN touches a package `SOURCES.txt` lists) | `native/webview-*/**`, `native/wpe/**` | WP3, WP4 | 20–24 (L) | after beta |

Totals: about 36–51 agent-days to both betas, 28–41 more for full parity
(text, blur, embedded web view). With three or four lanes the Linux beta is
about 2.5–3 calendar weeks out and Windows 1–1.5 weeks after it.

### WP5 notifications: status

Same behaviour on all three OSes: the daemon's `notifications` setting (Settings → Notifications → "Show agent notifications")
gates every post, `agentToasts.ts` decides what to announce (a turn that finished or failed, an approval or a question
waiting; nothing for the thread being read in a focused window), and a click raises the window and opens the thread.
"Notification access" asks the OS for its answer: macOS shows the system prompt, Windows reports the toast setting of the
AUMID (`denied` when the user turned Kybern off, `granted` otherwise), Linux reports whether a notification daemon answers
(`unsupported` when none does, with the reason in Settings). The button then sends a test notification.

| | macOS | Windows | Linux |
| --- | --- | --- | --- |
| Mechanism | `notifications.dylib` (UserNotifications), loaded with `bun:ffi` in a packaged `.app` | GPUI `show_system_notification`: WinRT `ToastNotification`s, `ToastNotifier` for the AUMID | GPUI `show_system_notification`: `org.freedesktop.Notifications.Notify` over one long-lived session-bus connection (`zbus`) with a signal listener |
| Identity | bundle id `dev.kybern.gpuix` | An **installed** copy (the exe beside `uninstall.exe`): `dev.kybern.gpuix`, the AUMID `packaging/windows/shortcut.ps1` puts on the Start-menu shortcut. A portable, dev or CI run: `dev.kybern.gpuix.dev` ("Kybern (dev)"), whose registry key GPUI writes (`HKCU\Software\Classes\AppUserModelId\dev.kybern.gpuix.dev`, DisplayName) so the run never recreates the installed app's key or shares its per-app toast setting. `main.tsx` calls `kybernSetAppIdentity` before `render`, so the process AUMID is set before the first window exists, in every environment window's process too (the addon applies it first thing on the UI thread) | app name `Kybern`, icon and `desktop-entry` hint `dev.kybern.gpuix` (the `.desktop` file and icon the deb, rpm and AppImage install) |
| Replace | one notification per id | one per thread: the toast tag (hashed); a toast with the same tag replaces the old one (no `Hide`, which failed once the user had dismissed the old toast and then dropped the new one) | one per thread: `replaces_id` of the server id the thread's last post got. One worker sends the calls in order, so two quick posts are one notification; a closed notification, or a restarted server (`NameOwnerChanged`), empties the thread's slot |
| Withdraw | `notifications.dismiss(group)` removes the group's delivered ids (`dismiss` op in `notifications.m`) | `Hide` of the thread's toast | `CloseNotification` of the thread's server id |
| Sound | the system's | system default; `<audio silent>` for a replacement of the same thread within 10 s | `sound-name` hint; `suppress-sound` for that replacement |
| Click | the delegate activates the app, then the click event | `ToastNotification.Activated` (WinRT thread) → UI thread → `cx.activate` + `activate_window` (restores a minimised window, foreground via GPUI's input trick) → JS | the default `ActionInvoked` (the one listener, matched to the thread by server id) → same path; a Wayland compositor may refuse to raise the window without an activation token, then the thread still opens |
| Focus Assist / Do Not Disturb | the system | the system: toasts go to the action center, sound is the system's (the toast has no `silent` audio) | the daemon's (`sound-name` `message-new-instant` is a hint it may ignore) |
| No service | not packaged: `unsupported` | not registered or disabled: `unsupported` / `denied`; the in-app toasts carry on | no daemon: `unsupported`, a debug log, nothing posted |

When the app is not running: the daemon outlives it, but a notification is posted by the app, so nothing new appears. A toast
that is still in the Windows action center does nothing when clicked, because the shortcut has no COM activator (that would
launch a fresh process with the toast's arguments; not built, an open item). A Linux notification whose app exited has no
listener, so a click does nothing. Click handling needs the app running; macOS relaunches the app, which is the one place it
differs.

Fork branches `wp5` (on top of the heads `native/FORK` pinned before; `FORK` pins them until they are merged):

| Fork | Commit | What |
| --- | --- | --- |
| `Tresnanda/zed` | "System notifications: replace per tag, app icon and sound (WP5)" | Linux: `replaces_id` per tag with a generation so only the newest post reports a click, the identifier as icon and desktop entry, the name as app name, the `sound-name` hint, no daemon logged at debug level. Windows: no `<audio silent>` |
| `Tresnanda/gpuix` | "System notifications: addon methods for Windows and Linux (WP5)" | `GpuixRenderer.setAppIdentity`, `showSystemNotification`, `dismissSystemNotification`, `setSystemNotificationCallback` (`{ tag, action }`; raises the window first), `systemNotificationStatus` (async); `src/system_notifications.rs` |

### WP5 review fixes (`wp5fix`)

Fork branches `wp5fix` (one commit each on top of the `wp5` heads: `Tresnanda/zed` `161896e395`, `Tresnanda/gpuix` `d0deeca3b3`; `native/FORK` pins them until they are merged).

| Finding | Change |
| --- | --- |
| Windows: a failed `Hide(previous)` dropped the new toast | `Hide` is gone from `show`: the same tag replaces the toast. `dismiss` still hides. `active_toasts` is shared with the response task and pruned when a toast is clicked (the only event that removes it from the action center; a swiped-away toast stays there, so it stays here to be hidden) and capped at 64 |
| `approval_resolved` closed only the in-app toast | `notifications.dismiss(group)` on all three OSes: macOS removes the group's delivered ids through the companion (`dismiss` op; rebuild `notifications.dylib`), Windows hides the toast, Linux sends `CloseNotification`. `agentToasts.ts` calls it when an approval is answered **if the thread's notification still announces that approval** (`shell/threadNotices.ts`; a later "Finished working" is not removed) and when the reader starts looking at a thread whose notification waits on an answer |
| Linux: a thread and a connection per post, stale `replaces_id` | One worker owns one session-bus connection (`zbus`) and sends `Notify` / `CloseNotification` in order; one listener thread on the same connection reads `ActionInvoked`, `NotificationClosed` and `NameOwnerChanged` for `org.freedesktop.Notifications`, keyed by server id. A server restart clears every slot; a lost bus starts a new service on the next post. Two posts for a tag before the first reply are serialised, so the second carries the first's id |
| Windows AUMID set after the first window | `kybernSetAppIdentity` (free addon function, read through the addon like `kybernCapabilities`) stores the identity at startup; the UI thread applies it with `set_app_identity` before `open_window`. `main.tsx` calls it before `render`; spawned environment windows run `main.tsx` too |
| Nits | `GetCapabilities` is asked with the status probe (`systemNotificationStatus` resolves `{ status, markup }`); the body is escaped only when the server advertises `body-markup` (unknown: escaped, as before). A replacement of a thread's notification within 10 s is silent (`showSystemNotification(.., silent)` → `suppress-sound` / `<audio silent>`). `notify-rust` is pinned `=4.18.0` (the addon's probe). A click after the 256-entry id map evicted its post reports the tag, and the app opens that thread (`knownThread`) in the current environment |

Checks added: `bun test src/native/notifications.test.ts src/shell/threadNotices.test.ts`; the Linux module's `live_server` test (opt-in, against `notify-server.py`: replace, quiet replace, quick double post, click, dismiss, server restart); `notify` check steps for dismiss, quick posts, restart, markup, and on Windows dismiss and a post after the action center was cleared. Not covered by any check: a real toast click, and that the process AUMID was in place before the first window (the AUMID itself is asserted through the registry entry and the history).

After the review fixes the `notify` check passes on windows-2022 and ubuntu-22.04 (run 40270725763 on `spike/wp5fix-1`) with every step listed under "Checks added", and locally on arm64 Ubuntu against the test server.

Proven in CI before them (run 40265425463 on `spike/wp5-a`, windows-2022 and ubuntu-22.04): Linux against the test D-Bus server
(`Notify` fields, `replaces_id`, `ActionInvoked` once per click, `unsupported` without a daemon); Windows through the real
toast API with the AUMID (status `granted`, the three posts return, the `AppUserModelId` registry entry exists, and the
action-center history holds two toasts for three posts: the thread's second post replaced its first). Not proven: a click
on a real toast (it needs a person at the shell; the user's PC was unreachable), a Linux desktop shell's own daemon, and
the Wayland activation token.

Checks: `bun test src/native/notifications.test.ts` (payload building, the per-OS backend choice and the Linux markup
escaping, tag/id mapping, status caching, "no daemon", all with an injected platform and fake host); the Rust unit tests
in `system_notifications.rs` and the zed Linux slot tests; `bun scripts/run-platform-checks.ts --checks notify`.

### WP9 backdrop blur and window materials: status

Fork branches `wp9` (`next` merged in; `native/FORK` pins them until they are merged into `kybern`). Native details:
[native/README.md](native/README.md#off-macos-wgpu-and-direct3d-11-wp9).

| Part | Linux (wgpu) | Windows (Direct3D 11) | macOS |
| --- | --- | --- | --- |
| Scene backdrop filter: copy behind the filter, horizontal pass (half size above 8 px), vertical pass writing `mix(destination, blurred, coverage)` | done, `backdrop` check passes on lavapipe | done, passes on WARP | unchanged Metal path; shares `gpui::backdrop_kernel` (`halfres-check`: half vs exact Gaussian mean 0.5, max 3, as before) |
| Window material | opaque window; the glass coats over the Solid frame colour (`desktopMaterial.ts` `none`); KDE's blur protocol not used | Windows 11 22H2+: Auto → Mica, custom blur → Acrylic (DWM system backdrop, frame extended, transparent clear); Windows 10: as Linux | vibrancy / adjustable blur, unchanged |
| App gating | `BACKDROP_BLUR` from `kybernCapabilities().backdrop` and the window's `backdropFilterSupported()` (false on a surface without `COPY_SRC`); GNOME high contrast and the portal's contrast turn scene backdrops off (`platform/osDisplay.ts`) | same, plus Windows' transparency effects and contrast themes | same (`BACKDROP_BLUR` is every patched build; NSWorkspace options) |

`backdrop` check (`native/backdrop-check.ts`, CI run [40265183786](https://github.com/Tresnanda/kybern/actions/runs/40265183786) at the pinned `wp9` heads, every check green on Linux X11, Wayland and Windows), the same lab on each renderer:

| Measure | macOS (Metal, M1) | Linux (lavapipe) | Windows (WARP) |
| --- | --- | --- | --- |
| Card row deviation behind the filter (raw stripes 127.5) | 0.5 | 0.5 | 0.5 |
| Card tint (expected 106, 126, 151) | 105, 125, 150 | 105, 125, 150 | 105, 125, 150 |
| Menu, blur 40 at saturation 1.5 (expected 181, 26, 181) | 181, 26, 181 | 181, 26, 181 | 181, 26, 181 |
| Filter off (`KYBERN_BACKDROP_FILTER=0`): card row deviation | — | 95.5 | 95.5 |

Frame cost over the busy scene (34 text rows over stripes, a paint animation keeping GPUI drawing; three filters: a 240×180 card at
blur 16, a 240×140 rounded-chain filter at blur 16, a 240×260 menu at blur 40). Software renderers draw on the CPU, so these are
relative only; macOS reports no frame counts (`getBridgeStats` is Linux/Windows) and its GPU cost does not show in CPU time.

| Configuration | Linux frames/s | Linux CPU ms/frame | Windows frames/s | Windows CPU ms/frame |
| --- | --- | --- | --- | --- |
| No surfaces | 20.8 | 134 | 24.1 | 118 |
| Surfaces, filter off (old fallback) | 20.9 | 148 | 23.9 | 123 |
| Surfaces filtered (default, half resolution) | 13.0 | 229 | 14.5 | 249 |
| Surfaces filtered, `KYBERN_BACKDROP_HALFRES=0` | 10.4 | 337 | 8.0 | 462 |

The cost scales with filter area only: each filter copies and blurs its own clip plus halo, never the frame, and the scratches
are pooled. On the software renderers the three filters add about 80 (Linux) and 125 (Windows) ms of CPU per frame at half
resolution, against 190 and 340 at full resolution. A first run (40263475348) gave the same pixels and similar costs.

Seen on a real Windows 11 desktop (satyamas, RTX 3060 + AMD iGPU, 2026-10-26): the blur on both GPUs, and Mica/Acrylic
tinted in Kybern's theme through `setWindowAppearance`, not the system's. A DirectComposition window shows the system
backdrop without the frame extension (identical pixels in the title band, rail and content), and the extension made DWM
paint its own minimize/maximize/close glyphs over the title bar, so the frame now stays unextended by default
(`KYBERN_DWM_EXTEND_FRAME=1` extends it) and Mica/Acrylic are on by default (`DWM_BY_DEFAULT`). Transparency effects and a
contrast theme changed while Kybern runs are followed by polling `osDisplay.ts` on Windows, which raises no window-state
event for them.

#### WP9 review fixes (forks' `wp9fix` branches)

CI run [40271249451](https://github.com/Tresnanda/kybern/actions/runs/40271249451) (spike branch, `checks: all`, lto off) at the
`wp9fix` heads: every job green on Linux X11, Wayland and Windows. `backdrop` there: the same card, tint and menu values as
above; the odd case at a real 721 x 481 device size keeps its last column and row (stripes under the half-opacity frost at
deviation 35.5 on lavapipe, 35.2 on WARP, half the raw 70.3); `backdropFilterSupported()` true, and false with
`KYBERN_BACKDROP_FILTER=0`; `setWindowAppearance` applied on Windows; `readOsDisplay()` read both options on Windows. macOS
(M1, local): the `backdrop` check passes, and its three captures are pixel-identical before and after the saturation clamp
(S5), the saturation-1.5 menu included.

| Finding | Change |
| --- | --- |
| S1 odd crop at half resolution | The crop is copied exactly; the blur scratch rounds up (`div_ceil`) and both passes clamp their reads to the copied size. A full-width frost on an odd-width window used to leave the last column uncopied, so wherever coverage was below 1 the vertical pass mixed a stale texel. New zed test (odd 201 x 121 target, half coverage; fails without the fix: last column `[0, 80, 0, 127]` instead of `[0, 160, 0, 255]`) and `backdrop` check case (`BACKDROP_LAB_ODD=1`, 721 x 481) |
| S2 Mica/Acrylic tint | gpui `set_window_appearance` now works on Windows: it sets `DWMWA_USE_IMMERSIVE_DARK_MODE` from Kybern's theme on every window (`setWindowAppearance`, called by `shell/appearance.ts`) and the `WM_SETTINGCHANGE` handler keeps it. The windows still report the system theme, for "System". Mica and Acrylic stay **opt-in** (`KYBERN_DESKTOP_MATERIAL=dwm`, `DWM_BY_DEFAULT` in `platform/desktopMaterial.ts`): CI cannot capture DWM, so the tint has not been seen on a real desktop. Flip `DWM_BY_DEFAULT` once it has (`native/window-material-lab.tsx` on the Windows PC) |
| S3 Reduce transparency / Increase contrast off macOS | `platform/osDisplay.ts`, kept apart from WP3's theme and reduced-motion reading: Windows reads both options on the window shell's poll (bun:ffi, no process); Linux reads GNOME's `high-contrast` live and the portal's `contrast` once (KDE Plasma 6 answers the portal). Settings names the OS's own option |
| S4 blur scratch at copy size | Copy and blur scratch pooled separately; the blur scratch is sized by the largest `scratch_size` |
| S5 premultiplied overflow at saturation > 1 | `rgb = clamp(rgb, 0, blurred_alpha)` after the saturation mix, on Metal, WGSL and HLSL in one commit |
| S6 `backdrop` always true | `kybernCapabilities().backdrop` is false with `KYBERN_BACKDROP_FILTER=0`; the renderer's `backdropFilterSupported()` reports the open window (`Window::backdrop_filter_supported`: wgpu needs `COPY_SRC` and no WebGL). `readWindowBackdrop` turns scene backdrops off, so frosted surfaces take dense fills |
| Nits | Crops under 2 px use full resolution (no 0 px scratch); D3D11 releases its scratches after 30 frames without a planned filter; wgpu `prepare` reuses its buffer and params bind group, and the horizontal pass clears instead of loading; Settings shows a Mica / Acrylic choice on Windows instead of the 1–64 slider, and the Sidebar only copy says "glass" where the window stays opaque |

### WP1 native platform core: status

Fork branches `wp1` (on top of `kybern`; `native/FORK` pins them until they are
merged into `kybern`):

| Fork | Commit | What |
| --- | --- | --- |
| `Tresnanda/zed` | "Frame wake hook off macOS (WP1)" | `gpui::set_frame_wake_hook` and `served_frame_count`, called from GPUI's frame-request closure when a frame drew or ran next-frame callbacks (not on macOS) |
| `Tresnanda/zed` | "Wayland: run without a seat, fall back to X11 (WP1)" | Optional `wl_seat` end to end; `WaylandClient::new` returns an error and `current_platform` falls back to X11 when `DISPLAY` is set |
| `Tresnanda/zed` | "WP1 review: per-window frame counters, Wayland seat report, X11 fallback on missing globals" | `Window::frame_counters()` (drawn vs callbacks-only, readable off the UI thread), `gpui::wayland_seat()`, the fallback also when `wl_compositor`, `wl_shm` or `xdg_wm_base` cannot be bound |
| `Tresnanda/zed` | "WGSL and HLSL: read the 40-byte rounded clip node with edge fades" | The fix for the first CI run (below): the shaders' `RoundedClipNode` lacked `kind`, so every clip chain past the first node was misread off macOS |
| `Tresnanda/gpuix` | "Windows and Linux: native frame wake, bridge cost, scroll phases, DPI info (WP1)" | `setFrameWakeCallback` returns true off macOS; detached `()` hooks; island invalidation; `getBridgeStats()`, `getPlatformInfo()`; `simulateScrollPhase` everywhere |
| `Tresnanda/gpuix` | "WP1 review: exit-only JS wake, frame counts, closed-window errors, capture as drawn" | Off macOS only the UI-exit wake; `getBridgeStats` per-window `drawnFrames`/`callbackFrames`; queued hooks fail on a closed window and log failures as errors; the frame clock delivers an acknowledge that missed a frame at once; `SetWindowTitle` notifies without `window.refresh()`; `getPlatformInfo` `seat` and the real DPI awareness (`dpiMode`, `dpiSource`); `closeWindow()`; `captureScreenshot(path, asDrawn)` |
| `Tresnanda/gpuix` | "Use the zed wp1 branch (WP1)" | The gitlink (bumped with each zed commit) |

Decided against in WP1: parking GPUI's X11 refresh loop and the Windows vsync
thread while idle. Both still wake the UI thread every vsync for a no-op frame
request (Wayland parks). That is upstream GPUI's idle cost, not the JS loop's;
the X11 loop also drains X events, and the Windows thread re-invalidates
deferred draws and checks for device loss, so it needs a Linux and Windows run
to change safely. `frame-wake-check` reports process CPU while idle so the cost
is visible.

What proves what:

- **macOS unchanged.** Every Linux/Windows change is `cfg`-gated; on macOS the
  gpui commits compile to nothing, `update_window_detached` calls
  `update_window`, and the display-link hook only gained a wake counter. The
  addon built from the pin passes `update.sh`'s unit tests, realign, signing,
  load check and arena tests; its napi surface differs from the `kybern` pin's
  by exactly `getBridgeStats`, `getPlatformInfo` and
  `TestGpuixRenderer.simulateScrollPhase`; the app's typecheck and 952 unit
  tests pass; both addons launch the app against a scratch daemon and show the
  seeded transcript (`platform-smoke.ts --capture none`). The review round's
  pin (`c379feb`/`6a76c18`) was checked on screen: `frame-wake-check` passes
  on macOS, repaint captures correct at both parities. On screen the macOS
  display link wakes the JS loop every vsync (60/s) even with nothing
  animating (the previous check script reports the same against this addon,
  and no macOS change here touches the display link), so that idle limit is a
  warning on macOS and a failure only on Linux and Windows.
- **Linux and Windows compile.** `cargo check` of the native crate for
  `x86_64-unknown-linux-gnu` and `x86_64-pc-windows-msvc` passes from macOS with
  stub C tools (`cargo check` never links the C dependencies, so a stub
  compiler satisfies aws-lc-sys, ring and the resource compiler). Nothing ran on
  Linux or Windows: OrbStack's Docker did not respond.
- **On screen: needs the CI run below.**

The CI run that validates WP1 (`.github/workflows/gpuix-platforms.yml`, manual;
`workflow_dispatch` needs the workflow on `main`, so before the merge use a
throwaway `spike/*` branch with a push trigger as the ADE-52 spike did), inputs
`lto: false` (enough for a behaviour check) and the default `checks: all`, `wayland: true`. Everything below lands in the
`gpuix-<job>-checks` artifacts:

| Step | Proves |
| --- | --- |
| Build the patched GPUIX addon (all three jobs) | The `wp1` pin builds and links for real (the local check never linked) |
| Smoke check on screen, `smoke-clip-1/*-app.png`, `smoke-clip-0/*` | The app and the clip lab draw (rounded clips included) with island invalidation; compare with the ADE-52 captures (`.kybern-artifacts/ade-grill/`) |
| Smoke transcript scroll, `smoke-clip-1/scroll.json` | Scrolling a 24-turn transcript waits on the UI thread < 0.5 times per served frame (the wheel input itself excluded), and the transcript moves |
| `frame-wake` (every job), `frame-wake/frame-wake-<platform>.json` | Idle: ≤ 15 JS ticks/s, no JS frame wake off macOS, process CPU recorded (warns over 25 % on CI); animating: ≥ 10 `onFrame` deliveries/s, ticks ≥ wakes, ≤ 2 JS wakes/s off macOS, < 0.5 waited bridge calls per frame, frames split drawn/callbacks-only; `simulateScrollPhase` moves a scroller; repaint: a plain box, a cached island's leaf and a cached island changed in the same batch as its parent show the colour of `frame % 2` in captures as drawn at both parities (`frame-wake-even.png`, `frame-wake-odd.png`); `closeWindow()` ends the JS loop through the UI-exit wake |
| `frame-wake-full` (X11 and Windows), `KYBERN_FULL_INVALIDATE=1` | The same repaint captures with the old full refresh: the control for the island path |
| Linux X11: `gpuix-checks-stale-wayland/frame-wake/frame-wake-x11.json` | With `WAYLAND_DISPLAY=wayland-missing` under Xvfb, GPUI falls back to X11 (`platform` `X11`, `seat` null) |
| `linux-wayland` job (headless Weston): `frame-wake/frame-wake-wayland.json`, `smoke-clip-1` with `--capture none` | `platform` is `Wayland` and `seat` is `false` (GPUI really ran without a `wl_seat`), the frame-wake checks without captures, and the whole app comes up with the seeded message |
| Windows: `frame-wake/frame-wake-windows.json` | Direct3D 11/WARP, `info.windows.dpiMode` a per-monitor-v2 mode (the thread's real awareness), `windowDpi > 0` and `scaleFactor ≈ windowDpi / 96` |
| "Assert the WP1 platform reports" (every job) | `scripts/assert-platform-reports.ts` lists each fact above on the run page and fails on a missing report |

Check steps run with `!cancelled()` once the addon built, so one failing step still leaves every report.
CI cannot change the display scale, so 125/150/175 % and moving between
monitors stay on the real-hardware list.

The first run of this pin (`spike/gpuix-platforms-1`, run 40246824783,
2026-10-26; artifacts in `.kybern-artifacts/wp1-ci/`): Wayland passed (frame
wake, app smoke); on X11 and Windows `frame-wake` passed but measured 62 JS
wakes/s while animating, the reason the per-frame wake went. The smoke, clip,
rounded and Markdown checks failed for three reasons that predate WP1:

- Content inside rounded overflow clips was missing (Windows): the Metal-only
  "Analytic rounded clip chains" commit grew `gpui::RoundedClip` to 40 bytes
  (`kind`, for edge fades) and the WGSL and HLSL nodes stayed 36. Fixed on
  `wp1` (commit table); in an arm64 Ubuntu 22.04 container under Xvfb the clip
  lab now matches the macOS capture.
- Every X11 capture was black: Xvfb's Composite extension offers a 32-bit
  visual, GPUI's window takes it and is redirected off-screen with no
  compositing manager to draw it back. The workflow starts Xvfb with
  `-extension Composite` (GPUI then uses a 24-bit visual).
- "Wrap must grow" read bounds a fixed 32 ms after a click, which off macOS can
  land before the JS handler, the batch and the next drawn frame;
  `clip-check` and `markdown-document-check` now wait for the reflow
  (`settled` in `native/check-harness.ts`).

With those fixed, in the container `clip-check`, `frame-wake` (X11, the full
refresh control, a stale `WAYLAND_DISPLAY`, headless Weston without a seat)
pass. Still failing there, not yet investigated: `rounded-check` ("First
external corner is rounded", a one-pixel sample at 1×) and
`markdown-document-check` ("Grid adds actual outer/vertical borders"); and in
CI `nowebview` timed out waiting for the seeded pictures on both platforms.

### WP3 window and input: status

Fork branches `wp3` (`next`, i.e. the ci-fix heads, plus WP3; `native/FORK` pins them):

| Fork | Commit | What |
| --- | --- | --- |
| `Tresnanda/zed` | "Windows: controls inside a caption area keep their press (WP3)" | `WindowControlArea::Client`; the window-control hit test takes the topmost area under the pointer, so a button in a custom title bar answers HTCLIENT, not HTCAPTION |
| `Tresnanda/zed` | "Windows: hit-test window controls where the pointer is now (WP3)" | The answer comes from a hit test at the cursor's position, not `mouse_hit_test` (as new as the last dispatched event, while Windows asks before the move or press that updates it); the last answer stands in when the app is borrowed |
| `Tresnanda/zed` | "Keep window-control areas of cached views (WP3)" | `reuse_paint` replayed a cached view without its `window_control_hitboxes`: the title bar (a cached island) lost its caption and buttons on every frame that reused it, so a press on maximize answered HTCLIENT. Found by the CI check |
| `Tresnanda/zed` | "Windows: give GPUI the release of a caption press (WP3)" | Caption and border presses no element takes run `DefWindowProc` (move/size loop, double-click maximize) in the handler and then dispatch the release the loop consumed, so GPUI's press state is balanced |
| `Tresnanda/gpuix` | "Windows and Linux: app-drawn title bar, window state, key interception (WP3)" | `windowControl` caption buttons, per-OS drag regions, `getWindowChrome` / `setWindowChromeCallback`, `getWindowBounds`, `startWindowResize`, `showWindowMenu`, `setTitlebarDoubleClick`, window options `windowDecorations`, `x`, `y`, `maximized`, `display`; the key interceptors off macOS; the editor's `secondary` shortcuts |
| `Tresnanda/gpuix` | "Window chrome: send the state with each change, bounds included (WP3)" | The callback carries the state, so following the window costs no waited bridge call |

App side:

- **Title bar** (`nav/TitleBar.tsx`, `nav/WindowControls.tsx`): `useWindowControlsLayout` gives the leading chrome its
  start (82 px after the traffic lights on macOS, 12 px elsewhere, after left-side Linux buttons) and the trailing
  toggles their end. Windows: 46 px caption buttons the bar's height, Fluent glyphs, neutral kit hover/press fills,
  close #C42B1C with a white glyph, dimmed glyphs while inactive; no `onClick` (Windows acts on its hit test). Linux:
  24 px round buttons with a faint fill (Adwaita), in `buttonLayout` order and side, minus what `controls` lacks;
  5 px resize bands and 12 px corners (`ResizeEdges`) unless tiled or maximized.
- **Shortcuts** (`platform/keys.ts`): one `isPrimary`; the conflict review is in its header and in `nav/keys.ts`: a
  focused terminal off macOS gets Ctrl+key and Alt+key (its copy/paste/find move to Ctrl+Shift+C/V/F); back/forward
  Alt+←/→, open parent Alt+↑, spaces Alt+1–9, focus group Alt+Shift+arrows (not Ctrl+Alt: GNOME workspaces, Intel
  rotation hotkeys, AltGr); Emacs ⌃A/⌃E/⌃N/⌃P macOS-only.
- **Window state** (`shell/windowChrome.ts`, `platform/systemPrefs.ts`): theme and reduced motion at launch and live,
  the frame saved and restored per display, the minimum size (900×600) on every OS, the close guard through
  `WM_CLOSE` / `closeWindow`.

The check is `scripts/window-chrome-check.ts` (`window-chrome` in `run-platform-checks.ts`): on Windows it drives the
caption with real input (`native/win-input.ts`), because synthesized automation events never reach the Windows hit
test; on Linux the workflow runs it a second time under xfwm4's compositor (client-side decorations need a compositing
manager with `_GTK_FRAME_EXTENTS`) with `xdotool`. Results: see [WP3 CI](#wp3-ci).

#### WP3 CI

Throwaway `spike/wp3-1` (WP3 commits on `feat/gpuix-platforms` plus a push trigger), 2026-10-26; artifacts in
`.kybern-artifacts/wp3/` of the main checkout. The final run, [40276978896](https://github.com/Tresnanda/kybern/actions/runs/40276978896),
passed on every job with every check:

| Job | window-chrome | Rest |
| --- | --- | --- |
| Windows (WARP, real input) | pass: 46×46 buttons flush top-right; red close hover; `WM_NCHITTEST` answers caption / minimize / maximize / close / client (a control in the bar); maximize and restore by the button (restore glyph while maximized); double click maximizes and restores; a drag moves the window by exactly the drag; a control in the bar keeps its click before and after a window drag; real Ctrl+K; minimize; close ends the app | all pass; `nowebview`'s Ctrl+2/3/1 step reports its `shortcut` gap closed |
| Linux X11 (Xvfb, no window manager) | pass: server-side fallback, no buttons drawn, 12 px leading edge, shortcuts | all pass |
| Linux X11 + xfwm4 compositor (`xdotool`) | pass: client-side decorations, GNOME buttons; maximize/restore, double click, drag, right-edge resize, minimize, close | — |
| Linux Wayland (headless Weston) | not run (no input, no capture) | frame-wake and smoke pass |

The earlier runs on the way found the three zed commits above: the cached title bar's lost caption areas (maximize
did nothing), the stale hit test, and the press GPUI never saw released. macOS on the final pin (`update.sh`, then
`run-platform-checks.ts --checks window-chrome,focus,clip,smoke,frame-wake` on this Mac): all pass, traffic-light room
unchanged (82 px), ⌘K / not ⌃K; the app's typecheck is clean and its 976 unit tests pass.

Not covered by CI (needs the real PC or desktops): Windows 11 Snap Layouts flyout and Aero snap on real DWM (the runner
is Server 2022), mixed-DPI caption hit tests, the system menu on right click, GNOME Wayland and KDE (portal
`color-scheme` / `gtk-decoration-layout` live updates), `enable-animations` and Windows Animation effects toggled
while running, and the restored frame across monitors.

#### WP3 review fixes

Fork branches `wp3fix` (one commit each on `next`; `native/FORK` pins them): zed "Windows: hit-test the
WM_NCHITTEST point, balance right caption presses, live Animation effects (WP3 review)", gpuix "Window chrome off
macOS: WP3 review fixes".

| Finding | Fix |
| --- | --- |
| AltGr text in the terminal (Windows sends Ctrl+Alt plus `keyChar`) | `terminalKey` sends the composed character off macOS (`isAltGrText`, `platform/keys.ts`) before the Ctrl branches; Ctrl+Alt without one is unchanged. Audit: composer Ctrl+V and Preview Ctrl+Alt+P use `isPrimaryChord`; Notes Ctrl+S/F, Pull requests Ctrl+F, login Ctrl+C, approval Ctrl+E and the Tasks page's Ctrl shortcuts skip Alt off macOS (`isPrimaryNotAltGr`); a key the native text field holds back for JS is replayed with its `prefer_character_input` (`Window::dispatch_key_down`), so a ctrl-alt editor binding (headings, code block) cannot take AltGr text |
| Hit test at the cursor, not the message's point | `hit_test_position` (the `WM_NCHITTEST` point during its hit test); the window-chrome check now probes with the cursor parked in the content |
| Composer Ctrl+V with Alt (Hungarian AltGr+V `@`) | `isPrimaryChord(m, { shift: 'any' })` |
| Orphaned `gsettings monitor` children | `watchGsettings`: one monitor per schema shared by all listeners (`osDisplay.ts` included), line-buffered, killed on exit, SIGTERM and SIGINT; the double-click watcher uses the value it gets. Launch reads: the registry through bun:ffi (no `reg`), one `gsettings list-recursively` for theme and animations, the double click read without blocking |
| Re-render per move/resize step | `bounds` are kept out of the React snapshot (`sameChrome`); the addon keeps reduced motion, the button layout and the display id between notifications and sends a bounds-only change once the frame rests (200 ms) |
| Linux launch theme overwritten by the first report | `takesAppearance`: on Linux the first report does not override a theme the launch read found; later changes do |
| X11 saved frame on an unplugged monitor | Off-display title bar → centered on the primary display (`window_chrome::on_a_display`) |
| Right caption press left open on Windows | `run_caption_press` balances the right button too |
| Animation effects not live on Windows | `WM_SETTINGCHANGE` `SPI_SETCLIENTAREAANIMATION` calls the appearance observers |
| Text fields off macOS | Ctrl+Y redo, Ctrl+Home/End (with Shift) |
| Ctrl+Alt+←/→ collisions | Alt+Shift+arrows off macOS (`isFocusGroupModifier`, `SHORTCUT.focusGroup`) |
| Linux right click on a title-bar control | `control()` marks presses of any button, so the region does not open the window menu |

The WP10 merge on `next` left two `cfg(linux|freebsd)` dependency tables in the native crate's `Cargo.toml`, which
cargo rejects; `wp3fix` folds them into one.

Follow-up (not done): client-side decorations on Linux draw no inset or shadow around the window.

CI (throwaway `spike/wp3fix-1` and `-2`, 2026-10-26): [40281977524](https://github.com/Tresnanda/kybern/actions/runs/40281977524)
(window-chrome, smoke, nowebview, focus): window-chrome passes on Windows (WARP, hit tests probed with the cursor in the
content), Linux X11 and under xfwm4; smoke and focus pass everywhere; Windows nowebview times out installing the preview
browser (the daemon connection drops), as it does on `feat/gpuix-platforms` without these commits (run 40283556066).
Strict, all checks: [40281980707](https://github.com/Tresnanda/kybern/actions/runs/40281980707): only the wheel gaps
(clip, Linux markdown-document) and nowebview (as above; Linux lacks the WPE pkg-config there) fail. macOS on the
`wp3fix` addon: window-chrome, focus, smoke (both clip modes) and clip pass.

### Where the forks help

**Status (2026-10-25): done.** The patch queue (`native/patches/*.rs`,
`apply_hooks.py`, `backdrop_gpui.py` and the 89 hook scripts they ran) is now
two commit series, one commit per former script, and was deleted from this
repository:

| Fork | Branch | Base | Commits |
| --- | --- | --- | --- |
| [`Tresnanda/gpuix`](https://github.com/Tresnanda/gpuix/tree/kybern) | `kybern` | tag `@gpuix/native@0.11.0` (`2e31b47`) | 58: the native crate (57) and the zed submodule pointer |
| [`Tresnanda/zed`](https://github.com/Tresnanda/zed/tree/kybern) | `kybern` | remorses/zed `gpuix` `f77e5cf` (what 0.11.0 pins) | 49: GPUI, Metal, WGSL and HLSL |

`native/FORK` pins the gpuix commit and the zed commit it records;
`native/update.sh` builds exactly that, and the CI addon cache is keyed on
`FORK`. A fresh clone of the pinned commit is identical to what the old queue
produced except `.gitmodules` (the fork URL) and `Cargo.lock` (the mimalloc
entries cargo used to write on the first build), and the macOS addon built from
it was byte-identical to the old one. Each commit body names the script it came
from: `git log --grep 'From rotate_gpui.py'` on the zed fork finds that patch.

Making a native change now (`native/README.md`, "Changing the native module"):
commit on `Tresnanda/zed` `kybern` (GPUI) and/or `Tresnanda/gpuix` `kybern`
(native crate; bump its `zed` gitlink when zed moved), push, set `commit` and
`zed` in `native/FORK`, run `native/update.sh`. Try unpushed commits with
`KYBERN_GPUIX_SOURCE=<gpuix checkout> ./update.sh`.

What that buys for the remaining work packages:

- Shader edits (`platforms_gpui.py` used anchored replacements and a regex that
  wrapped every fragment `return`) are now plain diffs to
  `shaders.wgsl`/`shaders.hlsl`, reviewable and rebased by git.
- The UI-thread bridge and the off-macOS fixes to upstream code
  (`elements_at`, painted-text readers) are upstreamable to GPUIX as commits.
- Each platform lane builds from the same pinned commit, and the cache key is
  the pin.
- Shader changes for Metal, WGSL and HLSL land in one commit, so a missing
  backend shows in review instead of as a blank Linux window.
- The weekly canary (`gpuix-canary.yml`) rebases both series onto a new
  upstream release and lists the commits that conflict.

## What needs real hardware or CI

- **Windows:**
  - D3D11 on Intel iGPU, NVIDIA and AMD
  - per-monitor DPI at 125/150/175 %, mixed-DPI monitor moves (`getPlatformInfo()`
    reports `scaleFactor`, `dpiMode` and `windowDpi` after each move; text and
    hit targets should stay sharp and in place)
  - Windows 10 (no Mica; WebView2 runtime may be missing) and Windows 11
  - SmartScreen and Defender behaviour for the unsigned installer
  - the installer on a clean Windows 10 (the app-local C++ runtime, `kybern` on Path, Add/Remove
    Programs; the runner image already has the C++ runtime in System32, so CI cannot show a missing
    DLL), and no console windows flashing when the daemon spawns `git`, `gh`, shells and agent CLIs
    (the Windows `cargo check` of `kybern-daemon` and `kybern-drivers` is blocked by aws-lc-sys
    off Windows, so the `CREATE_NO_WINDOW` code is exercised only by the Windows CI build)
  - that `tauri signer sign` writes `file:<installer name>` as the trusted comment (the updater
    refuses a signature that names another file; `verify-archive.ts` checks it before the upload)
  - toasts after install (AUMID)
  - the silent-update relaunch
- **Linux:**
  - GNOME Wayland, KDE Wayland and an X11 session
  - Mesa (Intel/AMD) and NVIDIA proprietary drivers, where wgpu Vulkan can be
    absent and fall back to GL
  - fractional scaling
  - no-keyring sessions (Secret Service missing)
  - a notification daemon
  - AppImage without FUSE
- **Intel Mac:** see above (real Intel GPU, dual-GPU switching, macOS 13–15).

CI proves build, load, unit tests and software-rendered on-screen checks
(lavapipe, WARP). It cannot prove GPU drivers, compositors, DPI, installers or
energy: the spike's screenshots say nothing about real GPUs.

### Real Windows 11 PC (2026-10-26)

Windows 11 26200, Ryzen 7 5800H, RTX 3060 Laptop + Radeon iGPU (one monitor on each), 2560×1440 + 1920×1080 at
100 %, WebView2 155. Native MSVC build of everything, real Unicode NSIS installer. The GPU was chosen per run with
`HKCU\Software\Microsoft\DirectX\UserGpuPreferences` (`GpuPreference=2` NVIDIA, `1` AMD); GPUI logs `Using GPU: …`
with `RUST_LOG=info`, which confirmed each.

- **Checks:** `run-platform-checks.ts --checks all` passes 12/12 on both GPUs (`webview` 36/36 with real user32 input).
  The wheel steps of `clip` and `markdown-document` raced the synthesized wheel on real GPUs until they captured with
  `captureSettled`; every wheel gap now passes on both. Twice the first fixture of a run timed out at startup right
  after new binaries landed (a cold start); reruns pass. One `markdown-document` run on the iGPU hit a 2 s automation
  clock timeout during the 700-line code case (1 of 4 runs).
- **Fixed from this pass:** the title bar did not move the window and nav-rail buttons did not click (`window_drag.rs`,
  fork `winhw`); bounds queries threw while minimized, which crashed the composer (`answer_after_frame`); agent
  notifications were never posted (`isWindowFocused` was `true` off macOS); a hidden chat's composer cut a hole in a
  Browser tab's page; the System theme ignored Windows dark mode; the webview check could click other apps; the
  uninstaller left an empty `%LOCALAPPDATA%\kybern-gpuix`.
- **Proven:** D3D11 on both GPUs, including a window rendered on one GPU and shown on the other GPU's monitor;
  WebView2 visuals (live, drafting, full screen), Mermaid, artifacts, the Browser tab with a dev server found by
  discovery, menus cut out of pages; install, the in-app update over a running app (signed with `tauri signer`, whose
  trusted comment is `file:<installer>`; locked files moved aside as `.old`; silent relaunch; the daemon restarted by
  version), uninstall with nothing left; Start-menu AUMID, `kybern` on Path; toasts posted under the AUMID; the D3D11
  debug layer (loaded) reported nothing in the backdrop, clip and odd-width frost runs on either GPU; no stale last
  column at odd widths; Mica/Acrylic with and without the frame extension, maximized, snapped, inactive, Transparency
  effects off and High Contrast.
- **Open:** caption buttons (WP3; and with the DWM frame extension DWM paints its own min/max/close glyphs into the
  top-right corner, visible through a translucent title bar; fixed: the frame now stays unextended unless `KYBERN_DWM_EXTEND_FRAME=1`); a toast click
  (needs a person: the banner and notification center were unreachable while a Windows Security prompt was open);
  following a dark/light switch while running; window-state events (`setWindowStateCallback` is a no-op off macOS,
  so idle memory release and visibility never react, and loops keep running while minimized); the full-screen viewer
  once drew a live visual at its inline width (not reproduced; `SetBounds` failures are now logged); the update
  artwork shows moiré (a 2560 px image sampled without mipmaps); DPI other than 100 %, Windows 10, SmartScreen.

### Real Windows 11 PC, second pass (2026-10-26)

The same PC (satyamas), the installed per-user build of this branch on a scratch daemon, real user32 input, on the NVIDIA
GPU and then the AMD iGPU (`Using GPU: AMD Radeon(TM) Graphics`), 100 %, German layout loaded only for the AltGr steps.
Forks: `winhw2` (zed `9a8ab04`, gpuix `9634843`). Evidence: `.kybern-artifacts/win-hw2/` in the main checkout.

| Area | Result |
| --- | --- |
| WP3 caption buttons | pass: hit tests answer minimize/maximize/close/caption/client; minimize, maximize and restore by the buttons; double click maximizes and restores; the close button closes the window (with a document tab open too, fixed below) |
| Snap Layouts, Aero snap | pass: the flyout on hovering maximize; a drag to the left edge snaps to half the screen; a drag off restores |
| Window menu on a right click | **fixed**: the caption opened no menu (DefWindowProc showed none, nor for a posted WM_CONTEXTMENU; Alt+Space did). zed `winhw2` shows `GetSystemMenu` with `TrackPopupMenu` on a right release on HTCAPTION and runs the choice; Maximize from it works and the next click (the rail) works. The menu is drawn light in the dark theme (follow-up: dark menus need the undocumented `SetPreferredAppMode`) |
| Title-bar controls | pass: sidebar toggle and back click normally (`client` hit test) |
| AltGr (German) | pass: terminal, composer (AltGr+Q types `@` and opens mentions, nothing is pasted) and the rich editor all read `@{}[]\|~€`; the user's layout list is unchanged afterwards |
| Ctrl shortcuts | Ctrl+K: **fixed** (the palette opened with its field unfocused, keyboard or pointer). Ctrl+C in the terminal: **fixed** (never interrupted anything, see below). Ctrl+Shift+C/V: pass |
| Alt+Shift+arrows | pass: the focus ring moves to the other group; the input language is unaffected (a test that sends the arrow without the extended-key flag loses Shift to the numpad emulation) |
| Animation effects | pass: turned off and on while running, the palette's entrance follows each change |
| Frame across monitors | pass: moved to the second (1080p) monitor, closed, relaunched there at the same frame |
| WP8 text | pass: Segoe UI Variable; a note with an ordered list to 10, task chips and inline code with an emoji (no overlap or clipping); the VS16 row in colour; JetBrains Mono 400–700 registered; small text crisp at 1x. Italic still draws upright (known); 🇮🇩 shows as letters (Windows' emoji font has no flags) |
| WP9 Mica by default | **fixed**: a new install got Acrylic (the shared default blur 64 is a custom radius), which also shows the windows behind; Windows now defaults to Auto, Mica (`defaultBlur`). No DWM caption glyphs at the top right |
| WP9 top row | **fail, open**: over a magenta window, the window's top pixel row shows it (159,47,159) with Mica or Acrylic; the side edges do not. Neither a 1 px nor the full DWM frame extension changes it |
| WP9 themes | pass: Kybern light on dark Windows and dark on light Windows; System follows a Windows switch while running, both ways |
| Transparency effects, contrast theme | pass: turned on while running, Settings says why Kybern stays solid within seconds; settings restored |
| Notifications | pass: posted while unfocused (`posted true`), listed under "Notifications from Kybern"; a real click on it in the notification center brought Kybern forward on the right thread. No banner appeared on this PC (its notification settings), so the banner itself was not clicked |
| Taskbar | pass: the first window's taskbar button is `Appid: dev.kybern.gpuix` (the identity is set before the window, `setEarlyAppIdentity`) |

Fixed in this pass:

- **(a) Window-state events off macOS** (gpuix `winhw2`): `setWindowStateCallback` installs GPUI bounds/activation/appearance
  observers and delivers `{ minimized, visible, focused }` (Windows: `IsIconic`, `IsWindowVisible`, `DWMWA_CLOAKED`; Linux:
  focus only). `native/windowState.ts` keeps the report, `visibility.ts` and Linux `isWindowFocused` use it. Measured on the
  installed app: minimize raises `[visibility] hidden` and restore `visible`; with the reconnect spinner running the process
  used 16.3 % of a core visible, 1.9 % minimized, 11.6 % restored; idle release runs after 60 s in the background
  (`released gpu=0`: Direct3D 11 has no idle release yet), working set 260 → 232 MB minimized on the iGPU. Covering
  windows are not detected (Windows has no occlusion event).
- **(b) AppUserModelID**: already set before the first window; confirmed on the taskbar (above).
- **(c) Update artwork moiré**: off macOS the card and dialog draw copies pre-filtered near their size (420 and 768 px);
  the 2560 px original sampled 8x smaller without mipmaps showed the moiré. GPU capture of both in the card box.
- **(d) Hovered sidebar title**: not a bug; it is the hover marquee (`.t-marquee`): clipped at the end, panned to show the
  tail, home again on leaving.
- **(e) Web pages in the strip under the composer**: not reproduced (live visuals freeze before reaching the composer; the
  strip shows transcript text, which floats under the composer by design). A live visual now also freezes before it slides
  under the request bars and gutter of the floating stack, not only the card (`composerCardRects`).
- **(f) Artifacts chip**: not reproduced in 18 real clicks (centre, edges, 30 ms after the dock opens).
- **(g)** Command palette focus (gpuix: an `autoFocus` element inside an `autoFocus` parent keeps focus); terminal
  Ctrl+C (the daemon, started detached as a new process group, ignored Ctrl+C and every shell inherited that:
  `SetConsoleCtrlHandler(NULL, FALSE)` before the first shell); terminal working folder (`\\?\D:\…` project paths made
  cmd.exe start in `C:\Windows`); the close button closed the active document tab instead of the window
  (`closeRequestSource` off macOS); the window menu (above); macOS no longer compiled the native crate at `next` (two
  functions had lost their Windows/Linux gate).

## Ship order

1. **Intel Mac**: shipped in v0.10.3.
2. **ADE-52 spike** (done): the patched addon builds for Linux and Windows and
   draws in CI (`gpuix-platforms.yml`); rounded clipping on both backends.
3. **WP0**: forks (done), cached addon lanes in `desktop.yml` (Linux and Windows in, ADE-78);
   internal artifacts only.
4. **Linux x64 beta**: WP1–WP5, WP6(L), WP7. AppImage is simple and previews
   degrade to the browser. Offered next to Tauri, which stays the default.
   WP6(L) is in (ADE-78): the next release publishes the AppImage, `.deb` and
   `.rpm` as a beta with today's fallbacks (table above); WP1–WP5 and WP7 remain.
5. **Windows x64 beta**: WP6(W) and the Windows halves of WP1/WP3. WP6(W) packaging and the
   updater are in (ADE-78, see above); the beta stays unannounced until WP1–WP4 and a run on real
   Windows hardware pass.
6. **Parity**: WP8 text, WP9 backdrop blur and Mica, WP10 embedded web view;
   visual pairs and perf through WP7's harness.
7. **Tauri retirement** (below): done after 0.18.0.

## Tauri retirement

What happened (2026-10-28): the website and installers already pointed only to
GPUIX, so the planned final Tauri release was skipped and `apps/desktop` was
removed outright. The code GPUIX still imported moved to `apps/gpuix/src/shared`,
its icons to `apps/gpuix/packaging/icons`, the branded background to
`assets/release-background.png` and the performance guide to
`apps/gpuix/perf/tauri`. CI lost the Tauri desktop and native WebKit jobs,
`desktop.yml` builds only GPUIX, and `latest.json` carries only the `-gpuix`
keys. Tauri installs stop updating at 0.18.0: a later manifest has a newer
`version` but no plain `darwin-aarch64`-style entry, so Tauri's updater fails
the check (target not found). Automatic checks fail quietly; a manual check
shows "Unable to check for updates". Nothing is reinstalled.

The plan this replaced, kept for reference. The final Tauri release (call it vT):

- **Every launch shows the popup:** "Kybern Desktop is no longer updated. Move to
  Kybern GPUIX". It offers one-click Install and open (`install_gpuix`, extended
  to download the Windows installer or the Linux AppImage), plus the manual
  command. `gpuixPromptRules.ts` gets a `deprecated` mode with no "shown once"
  record.
- **Its own updater checks are turned off.** This is required, not optional:
  `latest.json` keeps one top-level `version`, which will move past vT with
  GPUIX releases. A vT client polling it would then reinstall vT forever.
- **Installs from older Tauri versions still land on vT.** The manifest job
  stops building Tauri and copies vT's `darwin-aarch64`, `darwin-x86_64`,
  `linux-x86_64` and `windows-x86_64` entries from a checked-in
  `scripts/tauri-final.json`. Older Tauri clients, whose updaters still run,
  then update to vT and see the popup.
- **Then the rest of issue #113 follows:**
  - installers and the website default to GPUIX
  - README, `docs/`, `AGENTS.md` and `CLAUDE.md` mark `apps/desktop` retired
    (it stays as the visual reference until removed)
  - the Tauri CI and release jobs are deleted
