## Sparse transcript and sidebar lists

The internal `sparseVersion` extension combines GPUIX's `itemCount/windowStart`
with `windowIndices` for noncontiguous retained owners. React supplies only the
buffered viewport and explicitly held rows. `sparseKeys` contains unique semantic
IDs, `sparseEstimates` contains logical-pixel height hints, and their version changes
only when metadata changes. `sparseLayout` invalidates heights after a transcript
width/theme change. These props are owned by `src/kit/sparseList.tsx`; they are
not a public upstream GPUIX API.

`sparse_list.rs` retains measured heights by semantic key, preserves the logical
anchor across prepends/reorders, and updates only mounted child/focus/revision
records during a window change. Stable scroll frames skip that synchronization.
The GPUI SumTree retains offscreen measurements even when the React host goes
away; fallback rows use those heights rather than replacing them with a uniform
estimate. Hinted splices and focus changes preserve the unaffected tree and
anchor. Metadata and the height summary remain proportional to loaded logical
rows; fibers, retained hosts, bounds observers and native row construction follow
the viewport plus selection/focus/overlay/animation owners.

Code: the "Sparse React windows with semantic row identity" (gpuix fork) and
"Sparse list: SumTree scrolling, O(changed range) topology" (zed fork) commits,
and `sparse_list.rs` (formerly `sparse_list_hooks.py`, `sparse_list_gpui.py`). `KYBERN_SPARSE_LIST=0` is
the dense-slot comparison control. Ordinary lists do not enter this path.
Checks and paired measurements are in `perf/scroll2/`.

# Patched GPUIX native module

The client runs on our own build of `@gpuix/native`: the upstream release in
`VERSION` plus the capabilities below, built from two forks that carry them as
commits:

- [`Tresnanda/gpuix`](https://github.com/Tresnanda/gpuix/tree/kybern), branch
  `kybern`: the release tag plus our native-crate commits
  (`packages/native/src`), with its `zed` submodule pointing at
- [`Tresnanda/zed`](https://github.com/Tresnanda/zed/tree/kybern), branch
  `kybern`: the GPUI commit the release pins plus our GPUI, Metal, WGSL and
  HLSL commits.

`FORK` pins the exact commits; `update.sh` fetches them into the disposable
`upstream/` checkout and builds. Until 2026-10-25 the same changes were a patch
queue here (`patches/*.rs` copied into the crate, `apply_hooks.py` and
`backdrop_gpui.py` running anchored edits). The forks hold it as one commit per
former script, and each commit body names its script, so a `patches/<name>`
mentioned below or in `../perf` is found with
`git log --grep 'From <name>'` on the fork, or as
`packages/native/src/<name>.rs` (gpuix) or `crates/*/src/<name>.rs` (zed).

## Transcript row geometry and logical anchors

`transcript_scroll_hooks.py` extends the existing scroll8 geometry reuse. Each
virtual row uses its own retained subtree revision in the cache key, so a live
sibling does not invalidate settled rows. Existing width, scale, inherited style,
topology and measured-leaf validation remains in `scroll_taffy.rs`; prepaint and
paint remain fresh.

Post-paint element observations now include `scrollTop: [itemIndex, offset]` for
virtual lists. A cloned `ListState` shares the live handle, so deferred observers
see the logical anchor after paint. Unchanged anchors/bounds emit no callback;
this adds neither an idle timer nor a frame request. Transcript windowing observes
the list itself so a programmatic jump can mount a destination even when every
previous rich body has left the viewport.

The transcript enables `cacheLayout` but keeps ordinary prepaint/bounds. Its
cached-island path (`KYBERN_SCROLL_ISLANDS=1`) is an experimental control: the
large-jump/image-overlay verifier exposes stale endpoint bounds that hide the
message rail. This does not change the existing PR island default.

Checks: `perf/transcript/verify.ts` covers large jumps, rail, Unicode selection
and offscreen image-overlay owners; `Window.test.tsx` covers bounded body and
observer ownership. Run the fixture under `perf/transcript/lease.py`.

## Rounded overflow and Explorer motion (round 7)

`overflow: 'hidden'` plus corner radii now clips descendant quads, glyphs, SVGs,
images, paths, surfaces, selection washes and backdrop filters. Nested rounded
rectangles form an analytic intersection in the Metal fragment shaders; ordinary
containers add no offscreen layer or extra render pass. The same mask chain gates
native hit testing, including prepaint hit tests. A pooled immutable chain is
shared by scene primitives, with generations for recycled slots so stale cache
keys cannot match a new shape. Cached replay and scoped paint preserve the clip
table. Completed frames reclaim unreferenced chains; storage follows peak visible
clipped content rather than scroll history.

Post-paint `ElementBoundsEvent.roundedClips` carries the immutable mask geometry
to embedded WKWebViews. Their AppKit layer masks intersect these shapes before
overlay occlusions, and native hit testing checks that same final path. Shape
snapshots are shared with cached bounds rather than allocated for every element.
The extension is currently Metal/macOS, like scene-local backdrops; the stock npm
addon retains rectangular clips. Radii follow GPUI's existing circular-corner
representation and clamping, not CSS elliptical corner radii.
The expanded `ContentMask` changes native primitive record sizes. These rebuild
hooks target Metal; WGPU's packed WGSL/WebGL records need a separate port before
reusing this patched GPUI checkout with that backend. The app loader selects the
local addon only on macOS (arm64 or x64); other platforms retain their npm addon.

Implementation: `rounded_clip.rs`, `rounded_clip_gpui.py` (reconstructed by
`backdrop_gpui.py`), the backdrop shader/renderer, bounds snapshots and the embedded
webview bridge. `native/clip-check.ts` checks actual native corner pixels, masked
input, cached replay, virtual rows and code scroll/wrap in four modes.

```tsx
const caps = renderer.getMotionCapabilities?.()
// caps === { rotate: true, physicalSprings: true }; rotate means SVG rotation.
<motion.svg source={icon} initial={false} animate={{ rotate: open ? 90 : 0 }}
  transition={{ type: 'spring', stiffness: 360, damping: 32, mass: .6 }} />
```

`rotate` is degrees about the native SVG centre. `motion.svg` shares the div
wrapper's presence, generation, cancellation and completion behavior. The raw
`<svg motion={{ initial, animate, transition }}>` form also works. Arbitrary
div-subtree rotation remains unsupported. Physical springs use analytic
under/critical/overdamped solutions in seconds; a conservative displacement and
velocity envelope determines completion, without truncating at a zero crossing.
Supplying any physical parameter selects that model (defaults 100/10/1); values
must be finite and positive and settle within 60 seconds. Without them, the
existing duration/bounce form is unchanged. Reduced motion uses `{duration:0}`.
`explorer_motion_hooks.py` and `physical_spring.rs` reconstruct this capability;
`native/explorer-motion-check.ts` verifies pixels and geometry in all four modes.

## Retained subtree translation (round 8)

`renderer.setSubtreeTranslation(elementId, { translateX, translateY })` moves
an already painted subtree in logical pixels. It retains glyph atlas entries,
shadows, opacity, scene ordering, native hitboxes and clipping; the layout slot
does not move. Pointer updates do not build elements, run Taffy, increment
ancestor revisions or wake cached islands. The first call on a node that did
not opt in installs its scope through an ordinary render.

For declarative motion, a host div accepts `paintTranslate` with the existing
motion description (`initial`, `animate`, `transition`, `generation`). Only
`translateX` and `translateY` belong in its styles. Tweens, duration/bounce
springs and physical springs use the existing evaluator on native display
callbacks. `src/drag/PaintTranslate.tsx` supplies React generation/completion
handling; `src/drag/carry.ts` binds pointer input directly to the renderer.

```tsx
<div paintTranslate={{ initial: false,
  animate: { translateX: 0, translateY: offset },
  transition: { type: 'spring', duration: .3, bounce: .05 } }}>
  {children}
</div>
```

Native layout observations report visual coordinates while a surrounding slot
wrapper stays fixed. Selection/link layouts and AccessKit bounds follow the
translation. Raw masks are retained before intersection: ancestor masks stay
fixed and rounded masks established inside the subtree move. A full render
reconstructs the same scene from the current translation; removing the prop or
remounting the host clears its retained motion state.

Implementation: `subtree_translation.rs`, `subtree_translation_hooks.py`,
`subtree_translation_gpui.py`, `subtree_geometry.rs`, `subtree_scene.rs`,
`subtree_bounds.rs`, and `subtree_a11y.rs`. The native translation check compares
retained painting to a full render and exercises bounds, input, selection and lifecycle
in four modes, with and without region painting alongside an animated loader.
Primitive scope/mask provenance is shared with `Arc` and translated once per
context. Translation currently
reconstructs the typed scene arrays and calls `Scene.finish()`,
which advances `paint_generation`. Region rendering safely rebuilds its retained
target after geometry changes; tracking old/new damage is a future GPU optimization.
Rotation is not part of this API: a 1° subtree tilt requires transformed quads,
glyph/path transforms, inverse input coordinates and rotated clip boundaries;
the SVG rotation capability remains separate.

Proposed rotation extension (not implemented): add `paintTransform` and
`setSubtreeTransform(id, { translateX, translateY, rotateZDegrees, origin })`,
keeping the translation API as its zero-angle convenience form. `origin` defaults
to `[.5, .5]` in the subtree's untransformed bounds; positive angles are clockwise.
Compose `T(translation) · T(origin) · R(angle) · T(-origin)`, including nested scopes.
Angles use the existing tween/spring evaluator; reduced motion takes the target
angle immediately. The desktop task ghost supplies 1° wherever its reference
class applies. Observers report the axis-aligned envelope of transformed corners;
layout slots keep their original bounds. Hit testing and selection inverse-map
coordinates, while native caret/IME rectangles use the visual envelope. Primitive
vertices, shadows and inner rounded clip nodes need the same affine transform;
ancestor clips remain in window coordinates. Retained GPU scope transforms must
preserve painter order and cached replay without Taffy or island invalidation.

## Cached views

GPUIX rebuilds and lays out every element of the window on every frame. Any
animation (a loader tick, a 220 ms collapse) therefore pays for the whole
window. Zed and zeron avoid that with GPUI cached views: an unchanged view
replays its previous frame. The patch exposes that to React:

```tsx
<div cache style={{ width: 14, height: 14 }}>…</div>
```

In `scripts/island-lab.tsx` (300 rows, a loader ticking 10×/s) CPU drops from
9.4% to 3.9% with the rows and loader as islands. In the app (title bar, rail,
thread panel and content card are islands; 15 s samples, 2026-10-15):

| Scene | Loader ticks/s | Stock | Patched |
|---|---|---|---|
| Home, a thread running in the sidebar | 10 | 7.6% | 4.9% |
| Home, a thread running in the sidebar | 5 (default) | 4.3% | 2.8% |
| That running thread open | 10 | 10.7% | 9.9% |
| That running thread open | 5 | 6.0% | 6.0% |

These historical measurements predate virtual-row cache support. Current list-row
measurements and the resolved width-measurement issue are recorded below.

## Island rules

- **Definite size:** `width` and `height` in pixels or percent. Without both the
  element renders inline (no caching, no error). `cache="fill"` (`FILL_ISLAND`)
  is for a box its parent sizes (flex-grow, stretch).
- **Virtual-list rows are supported.** Every native row establishes full width
  before measuring percentage-sized islands; the cache still needs definite height.
- **No margins or position insets on the island itself;** put them on a parent.
  The island keeps its declared size (it does not flex-shrink).
- **Unchanged islands replay.** General changes (props, text, children, geometry, motion) invalidate the
  owning cached island and its cached ancestors. The scoped-paint path below
  skips render/layout for its narrow fill/opacity allowlist.
- **Text selection** and painted-text inspection are restored in document order
  on cached replay. Selection changes refresh the native paint, including the wash.
  Deferred overlay text is not included in these ordinary-paint snapshots; keep
  selectable overlay content outside a cached ancestor.
- **Automation** (`getByTestId`, bounds) keeps working: islands snapshot their
  bounds and re-record them when replayed.
- **New islands render inline on their first frame;** their view is created
  right after it. (Creating or touching a cached view's entity during a draw
  stops GPUI from tracking it, and it would replay stale paint forever.)
- `KYBERN_DEBUG_ISLANDS=1` logs wakes and re-renders.
- With the stock npm binary (`KYBERN_GPUIX_NATIVE=npm`), `cache` is ignored and
  everything renders inline.

## Files

| Path | What |
|---|---|
| `VERSION` | Upstream `@gpuix/native` release the forks are based on (the npm packages) |
| `FORK` | The pinned `Tresnanda/gpuix` commit, the zed commit it records, and its upstream tag |
| fork `packages/native/src/cache_island.rs` | The feature: island views, invalidation, bounds replay |
| `../scripts/island-lab.tsx`, `../scripts/island-check.ts` | The lab and the automated check (identity, hover, animation, late updates, lists) |
| `patches/patch_js.py`, `patches/patch_react.py` | Native bridge, motion identity, declarations for the npm JS packages; reproducible pnpm patches in `../patches/`. The only anchored edits left (`patches/hook_check.py` gives them `--check`) |
| `tools/align_strtab.py` | Fixes Xcode 27 ld's misaligned LINKEDIT string pool, which dyld rejects |
| `tools/gpuix_canary.py` | Rebases the fork commits onto a new upstream release and reports conflicts (weekly in CI) |
| `update.sh` | Fetch the pinned fork → build → unit tests → repair → sign → load check → capability checks |
| `upstream/`, `build/` | Git-ignored checkout and build output |

## Changing the native module

A native change is a commit on a fork, then a new pin:

```sh
git clone --filter=blob:none --recurse-submodules --also-filter-submodules -b kybern \
  https://github.com/Tresnanda/gpuix.git ~/src/gpuix
cd ~/src/gpuix/zed && git switch kybern     # the submodule checks out detached
# GPUI / Metal / WGSL / HLSL: commit in zed/. Native crate: commit in packages/native.
KYBERN_GPUIX_SOURCE=~/src/gpuix apps/gpuix/native/update.sh   # build and check it unpushed
git -C ~/src/gpuix/zed push origin kybern
git -C ~/src/gpuix add zed && git -C ~/src/gpuix commit -m "Bump zed"   # when zed moved
git -C ~/src/gpuix push origin kybern
```

Then set `commit` (and `zed`) in `FORK` to the new heads, run `update.sh`, and
commit `FORK` with the app change that needs it. `update.sh` refuses a pin whose
gpuix commit records a different zed commit than `FORK` says. Only add commits
on top: a pinned commit has to stay fetchable. A Metal-only GPU change needs its
WGSL/HLSL counterpart in the same commit, or Linux/Windows draw it wrong.

## Moving to a new GPUIX release

1. Keep the current heads reachable: tag them `kybern-<old version>` in both
   forks (`git tag kybern-0.11.0 kybern && git push origin kybern-0.11.0`).
2. zed, when the new tag pins a different zed commit `Z'`:
   `git rebase --onto Z' <old zed base> kybern`, resolve, `git push --force-with-lease origin kybern`.
3. gpuix: add `remorses/gpuix` as a remote, fetch the tag, and
   `git rebase --onto @gpuix/native@<new> @gpuix/native@<old> kybern`. The last
   commit sets the zed submodule: resolve it to the rebased zed head. Push.
4. Set `commit`, `zed` and `upstream` in `FORK`, then

```sh
apps/gpuix/native/update.sh 0.12.0   # new upstream release
```

builds the pin, bumps `@gpuix/react` + `@gpuix/native` on npm, recreates the JS
patch and runs the checks. Run the app's usual checks and commit `VERSION`,
`FORK`, `../patches/` and the lockfile.

`tools/gpuix_canary.py --version <new> --keep <dir>` does steps 2 and 3 in a
throwaway checkout and lists every commit that conflicts, before you start. The
JS patches still use anchors: `python3 patches/patch_js.py <dir of the npm
package> --check` (also `patch_react.py`) prints every `MISSING <file>: expected
N anchor(s), found M` at once and exits 1 (`patches/hook_check.py`).

**Weekly canary.** `.github/workflows/gpuix-canary.yml` runs
`tools/gpuix_canary.py` every Monday and on demand. It skips when npm's latest is
`VERSION`; otherwise, on a Linux runner (a couple of minutes, no cargo), it
rebases both `kybern` series onto the new release in a throwaway checkout,
listing each commit that conflicts with its files (a conflicting commit is
replayed with our side, so the next one reports only its own collision), runs the
JS patches' `--check`, and lists the napi methods the release added that one of
our commits also defines (a clash is a compile error, and one clash hides the
next). When all of that is clean, a macOS job rebases again with `--tree`, runs
`KYBERN_GPUIX_SOURCE=<tree> KYBERN_NATIVE_CHECKS=0 update.sh <latest>` and the
app's typecheck and unit tests, which is what a static check cannot see. The
result is one issue labelled `gpuix-canary`: "our commits conflict" with the
list, "the build fails", or "ready to adopt"; it closes itself when `VERSION`
catches up. The canary never pushes. Locally:
`python3 -I tools/gpuix_canary.py --version 0.12.0 --report /tmp/canary.md`, or
`--ref main` to preview upstream's next release.

Prerequisites: Rust 1.97.1 (from upstream's `rust-toolchain.toml`) and Xcode's
Metal Toolchain (`xcodebuild -downloadComponent MetalToolchain`). The first
build takes a few minutes and about 2 GB in `build/`.

Intel Macs: `KYBERN_GPUIX_TARGET=x86_64-apple-darwin ./update.sh` cross-builds
`build/gpuix-native.darwin-x64.node` on Apple Silicon (`rustup target add
x86_64-apple-darwin --toolchain 1.97.1` first). Its load check and on-screen
checks run under Rosetta with the x64 Bun from `tools/bun-x64.sh`, which the
script puts first on `PATH`. The webview companion is universal (arm64 + x86_64,
macOS 13.0), so both arches share `build/kybern-webview.dylib`. Rosetta runs need
macOS 27 or earlier (macOS 28 drops it for apps); package local Intel apps with
`--test` (`packaging/README.md`). What Rosetta does
not prove, and the Windows/Linux plan: `../PORTING-PLATFORMS.md`.

Linux and Windows (ADE-52 spike): run `update.sh` on that OS with
`KYBERN_NATIVE_CHECKS=0` (Git Bash on Windows, with `PYTHON=python` and
`PYTHONUTF8=1`). It writes `build/gpuix-native.linux-x64-gnu.node` (or
`linux-arm64-gnu`) and `build/gpuix-native.win32-x64-msvc.node`, which
`src/native/local-build.ts` loads. Two fork commits (formerly the drivers that
ran last) only touch Linux/Windows code:

- "Linux and Windows: UI-thread bridge (ADE-52)" (gpuix, from `platforms_hooks.py`): off macOS, `update_window` and
  `update_window_without_view` box the hook's closure into `UiCommand::Run`,
  run it on the `gpuix-ui` thread and wait for its result, so a hook is
  written once and only has to be `Send + 'static`. Keep paint-time
  thread-local reads inside the closure: outside it they read the JS thread's
  empty copy.
- "Linux (wgpu) and Windows (Direct3D 11) build and draw (ADE-52)" (zed, from
  `platforms_gpui.py`): backdrop batches draw nothing on wgpu and
  Direct3D; `ContentMask` and the rounded clip chain walk in WGSL and HLSL
  (`KYBERN_ROUNDED_CLIP=0` squares every chain, for before/after checks);
  compositor loops stay in the scene. A new Metal-only GPU change needs its
  WGSL/HLSL counterpart, or Linux/Windows draw it wrong.

WP1 (`wp1` branches, `../PORTING-PLATFORMS.md#wp1-native-platform-core-status`)
added, still Linux/Windows only:

- "Frame wake hook off macOS (WP1)" (zed): `gpui::set_frame_wake_hook`, called
  after a frame request that drew or ran next-frame callbacks. The native
  crate turns it into `setFrameWakeCallback` (true on every desktop platform),
  so the JS frame loop runs on native frames there too.
- "Wayland: run without a seat, fall back to X11 (WP1)" (zed).
- "Windows and Linux: native frame wake, bridge cost, scroll phases, DPI info
  (WP1)" (gpuix): `()` hooks that need no answer use `update_window_detached` /
  `update_window_without_view_detached` (queued, not waited for; on macOS the
  plain call). Use them for new hooks that return nothing and run often.
  `UiCommand::Invalidate` takes the island path like macOS
  (`KYBERN_FULL_INVALIDATE=1` restores `window.refresh()`). `getBridgeStats()`
  and `getPlatformInfo()` are for checks and `perf/bench`.

`.github/workflows/gpuix-platforms.yml` builds both and runs the WP7 checks on
screen (Xvfb + lavapipe, D3D11 on WARP), including `scripts/platform-smoke.ts`
and `scripts/frame-wake-check.tsx` (check id `frame-wake`) on X11, headless
Weston and Windows. On a
Mac, an arm64 Linux container gives the same check in about a minute.

## The 0.11.0 port (from 0.10.0)

0.11.0 has no GPUI rebase (the Zed fork moves 18 commits: `f77e5cf`), the same
Rust 1.97.1, and a new `packages/gpui-terminal` crate that we do not use. Every
GPUI-side anchor still matched; `--check` found 17 problems in the native crate (14 moved
anchors, 2 cascades from them, 1 changed count) and 1 in the JS patch. What was done with each, so the next port can tell a
dropped patch from one that only changed its anchor:

| Patch | 0.11.0 | What changed |
|---|---|---|
| `register_font` (`apply_hooks.py`) | **dropped** | Upstream's `registerFont` (with font validation) is the same call. The app already guarded on it. |
| `set_frame_wake_callback`, `acknowledge_frame_wake`, `frame_clock::set_waker`, the display-link hook and the `runtime.js` wake edit (`frame_*`, `patch_js.py`) | **dropped** | Upstream wakes the JS loop from the display link itself (`set_frame_wake_hook`, `setFrameWakeCallback(loop)`, on unless `frameMs` is set). Wiring ours too woke the loop twice and clashed on the napi name. `KYBERN_VSYNC_WAKE` is no longer read; `subscribe_animation_frames` stays. |
| `onKeyDown` type edit (`patch_js.py`) | **dropped** | Upstream's `KeyEvent` has `preventDefault`/`defaultPrevented`/`stopPropagation`. |
| `interceptKeys` dispatch (`patch_js.py`) | reworked | Builds the intercepted event with upstream's `toKeyEvent` flags. Its verdict still goes back through `resolveKeyEvent` (queued off macOS since `key-detach`: never waited for, and a bridge error is logged instead of thrown out of the key dispatch), and its `stopPropagation` in native keeps the window's Tab default from running for an intercepted key. |
| Island fields, `BuildCtx` init, root prune, `build_element` lookup (`apply_hooks.py`) | re-anchored | New fields (`paint_reports`, `keyboard_focus`), `let tree = ctx.tree`. `cache_island.rs` builds its `BuildCtx` with both; `paint_reports` and `keyboard_focus_path` are `pub(crate)` for it. |
| Element accessors (every `.rs`/`_hooks.py` that read `custom_props`/`events`) | mechanical | They moved behind `extras`: `custom_props()`, `events()`. |
| Bounds replay in `cache_island` | kept, needs nothing | Upstream records host bounds with `on_painted` (`track_host_bounds`) instead of a `bounds_tracker` child. The island's snapshot-after-paint and replay-into-gaps canvases are independent of how a bound is recorded. |
| Clipped bounds, rounded masks, subtree geometry (`apply_hooks.py`, `subtree_translation_hooks.py`) | reworked | `bounds_tracker` used to carry them. They are now applied inside `track_host_bounds` (`record_clipped_bounds`, `record_start_region` with the window's subtree geometry). Bounds are the border box now (the old child filled the padding box). |
| `selection_surface_hooks.py` | reworked | Custom surfaces pass `selection_start_flag` into `track_host_bounds` rather than into a child canvas. |
| Dirty batches (`paint_hooks.py`, `subtree_*_hooks.py`) | reworked | `apply_batch` now invalidates only when the tree reports `dirty`. Our paint-only paths (`paint_updates`, `paintTranslate`, `paintTransform` retains) skip `mark_render_changed`, so they set `dirty` themselves. Without it a retained translation never repaints. |
| `has_fill` (anchored) | re-anchored | Upstream now keys on `declares_fill`, so an explicit transparent fill opts out of the forced opaque backing. |
| `dragOver`, `scroll` list accessibility, code-small destroy path, idle-memory pump | re-anchored | `style.active`/`focus_visible` neighbours, `let list = …;`, the `if dirty` block, `EMBEDDED_PLATFORM` (was `MAC_PLATFORM`). The window-state observer in `gpui_macos.rs` now anchors on `pub use platform::MacPlatform;`. |
| `hover_reconcile`/`hover_owner`, `layout_observer.rs`, `key_intercept.rs`, `idle_memory` | kept | Patched code unchanged and compiled against 0.11. Upstream also added "hover ends when the pointer leaves the window" and `onLayout`; the hover-fade and link-hover checks could not run after the restart (below), so the interplay is verified by `pointer-check`, `link-check` and the click checks only. |
| `keyboardFocusDim` default (`patch_react.py`) | new | Off unless a root passes `keyboardFocusDim: true`. |

App side: `SettingsScreen` no longer calls `focusNext` itself (the window does
now; it only stops the default while a settings overlay owns Tab);
`ImageMorph` keeps its own trapped Tab and stops the default so focus does not
move a second time behind the modal; the terminal stops the default for every
key it forwards to the shell. Tab navigation is otherwise left on: a Tab the
composer's `interceptKeys` consumes never reaches the window handler.

## Window drag regions

`<div windowDrag>` marks an empty chrome area. The native hook uses
`WindowControlArea::Drag` and, on macOS, mouse-down calls
`Window::start_window_move()` (AppKit `performWindowDragWithEvent:`). A second
click calls `Window::zoom_window()`. This is a native gesture, not JS position
updates. The title bar opts in; its buttons and rename target explicitly use
`pointerEvents: 'auto'` to occlude the drag hitbox. A custom interactive child
inside a drag region must do the same, except a menu trigger's content: it
would occlude the trigger itself, so the press never opens the menu (ADE-67:
Hand off). A drag region must also never be painted over sibling controls:
bubble listeners run topmost first, so its mouse-down starts the window move
and stops propagation before the controls below see the press (ADE-67: the
split title bar).

Anchors: `lib.rs` → `mod cache_island;`; `renderer.rs` → `// Wire up events.`.
Implementation: `patches/window_drag.rs`; React forwarding includes `motion.div`.

`native-check.ts` verifies that controls remain clickable. The separate
`window-drag-check.ts` posts real events to its own lab, checks exact 60×36px
movement without resizing, double-click zoom, then a button click. It requires
macOS event-posting permission and refuses to click a covered window. This
shell reports `CGPreflightPostEventAccess() == false`; the physical gesture is
**not verified here**. Run that check from an already authorized terminal.

## Input keys before native actions

```tsx
<textarea interceptKeys={['up', 'down', 'enter', 'tab', 'escape']}
  onKeyDown={event => {
    if (menuHandles(event.key)) event.preventDefault()
  }} />
```

The editor owns an `App::intercept_keystrokes` subscription. It suppresses an
opted-in native binding, and a first root key listener retains the complete
`KeyDownEvent` (including `isHeld`, absent from the interceptor API). A token
crosses the existing JS event bridge. The handler synchronously decides with
`preventDefault()`; a `finally` block returns that decision. Native default
handling is replayed only if focus still belongs to the original editor.
Following keystrokes queue behind the decision to preserve typing order.
Marked IME composition bypasses interception. The queue is bounded to 256;
if JS is stalled beyond that, oldest entries are dropped instead of growing
memory. No polling timer or blocking JS callback is introduced.

Anchors: `element_tree.rs` → `pub is_held: Option<bool>` and `is_held: None`;
`renderer.rs` → `with_window_menu_actions(root)`, `pub fn zoom_window(&self)`;
`custom_elements/input.rs` → `state: Option<Entity<TextEditorState>>`,
`emits_key_down: bool`, `let prop_changed = ...`, `state.emits_key_down = ...`,
`"readOnly" =>`, `"maxRows",`, `self.state = None`, and
`editor.on_key_down(move |event, _window, _cx| {`.
`patch_js.py` hooks `dispatch(event)` and adds the reply method declaration.

The composer now handles menu arrows, Enter, Tab and Escape on key-down,
including repeat, and prevents only consumed actions. Modified arrows continue
to native editing. `native-check.ts` verifies all five cancelled actions,
held keys, unprevented Shift+Enter, and a batched Shift+Enter/character sequence. Native undo is unchanged. Full IME
integration with a real input method still needs a manual pass.

### Caps Lock in replayed keys

A replayed key is inserted from GPUI's `key_char`, not from AppKit's text.
GPUI builds a macOS `key_char` with `UCKeyTranslate` from Shift and Option
only, so with Caps Lock on it was lowercase while ordinary typing (AppKit's
`insertText:`) was uppercase. Typing with Caps Lock into the composer came out
mixed: the letters it intercepts (`c x z y v`) always lowercase, and in a fast
burst every key queued behind an intercepted one too. The same `key_char`
reaches JS as `keyChar` (terminal input) and the held-key path.
`caps_lock_gpui.py` adds Caps Lock (`alphaLock`) to the modifiers of
`key_char` in `gpui_macos/src/events.rs` `parse_keystroke`; the binding `key`
is unchanged, so shortcuts still match the unmodified character.

`caps-lock-check.ts` sends AppKit key events with
`NSEventModifierFlagCapsLock` to its lab window (`caps-lock-input.m`, in
process, no global posting) and types into a plain input, the composer's
`TokenTextarea` with its `interceptKeys`, and the notes editor, one key at a
time and in a burst, plus Shift and unmodified controls. Before the fix it
reported `cAPS LOcK vxyz` (slow), `caps lock vxyz` (burst) and `ABcd` (notes,
behind an intercepted Escape). It assumes a Latin layout with letters on the
ANSI keys; an IME input source such as Japanese sends printable keys to the
input method first and never reaches the replay.

## Window close decisions

```tsx
import { setWindowCloseHandler } from '@gpuix/native/host'
useEffect(() => setWindowCloseHandler(renderer, async () => {
  return await confirmThreadsStillWorking()
}), [renderer])
```

`Window::on_window_should_close` vetoes immediately, emits a request, then
`resolveCloseRequest` removes the window only for the current allowed token.
The Window-menu/⌘W action goes through the same guard. Pending attempts coalesce;
Promise rejection vetoes. Cleanup removes the handler. This exposes the hook;
it does not install the app's close-guard dialog. The current renderer owns one
window; the guard is thread-local, not a multiwindow API.

Anchors: `renderer.rs` → `.on_action(|_: &CloseWindow, window, _cx| window.remove_window())`, and
`pub fn zoom_window(&self)` for `setCloseGuard`/`resolveCloseRequest`.
Implementation: `patches/close_guard.rs`; JS `dispatch(event)`/`STATE_KEY`.
`native-check.ts` verifies ⌘W veto, AppKit `performClose:` veto (the traffic-light
close path), continued interaction, Promise rejection, and successful close after permission.
This does not add an app-quit (⌘Q) guard.

## Deferred overlays inside islands

The old snapshot canvas ran before anchored/deferred children painted. After a
cached replay their automation bounds were absent ("Element has no painted
bounds") and clicks could not target them. The snapshot is now itself deferred
at the last priority, after the island's overlays. GPUI still owns hitbox replay;
our snapshot restores the retained-tree bounds used by automation.

The replay canvas must only restore **missing** bounds. On a changed island,
ordinary children have already painted their new coordinates, but the deferred
snapshot has not run yet. Unconditionally restoring the previous snapshot here
overwrote those new coordinates and captured the stale values again. The full
shell's Settings switch then received automation clicks at its old position;
Space still worked, and dock entrance checks read stale widths. This was an
automation-bounds regression, not the title-bar drag hook swallowing input.

The isolated bisect retained every other native extension: restoring the original
non-deferred snapshot made Settings clicks pass (but lost overlay bounds). The
focused regression moves a cached child 40px: the broken extension build reports
x=0 instead of x=40. Filling missing entries only preserves both fresh ordinary
bounds and deferred overlay replay. No hit-testing, drag, key, or motion behavior
was disabled.

`cache_island.rs` → `IslandView::render` and the replay canvas in `island_element`,
no new upstream hook.
`island-overlay-check.ts` first reproduced the missing bounds, then passed
three outside updates/cached replays, moved child coordinates/clicks, and overlay clicks. The normal five
island checks still pass. Their animation fixture uses a controlled clock so
another window covering the lab does not suppress the test animation. Overlays need not move to the app root solely to
avoid this failure. Ordinary text selection now survives replay (see selection API below).

## Secure single-line input

`<input secure>` keeps the edit buffer and onChange value in application memory,
but shapes bullets instead of its contents, maps Unicode byte offsets between
the two representations, and supplies a `PasswordInput` accessibility role and
masked value. Copy and cut do nothing. Automation inspection removes `value`
and `aria-valuetext`; painted text and `textContent()` expose no plaintext.
The prop applies only to single-line input. Do not also place the secret into
labels, child text, logs or other application-controlled properties.

GPUI has no secure-event-input API in this pinned fork. A balanced Carbon
`EnableSecureEventInput`/`DisableSecureEventInput` lease follows editor focus,
window activation and entity destruction. No new timer is used.

Anchors: `custom_elements/input.rs` → `impl TextEditorState`, editor construction,
`state.emits_key_down = ...`, `let default_role = if self.multiline`,
`if ctx.props.get("aria-valuetext")...`, `fn copy`, `fn cut`,
`let (display_text, is_placeholder)`, `fn point_for_index`,
`return (line_start + index).min(self.content.len())`, and the marked-range runs;
`retained_tree.rs` → `obj.insert("customProps"...)`.
Implementation: `patches/secure_input.rs`.

`secure-input-check.ts` passes with the nonsecret fixture `a😀é`: no value in
inspection/paint, unchanged clipboard after copy/cut, correct Unicode deletion,
and undo restoration. `NATIVE_AX_TEST=1` also requests an external AX-tree
inspection. It is blocked here (`AXIsProcessTrusted() == false`). The native
AX tree and secure-event-input activation/release still need an authorized
OS-level check before treating this as fully verified secret-entry UI.

## Highlighted code wrapping

`<code softWrap>` retains the existing highlighted `TextRun`s and native text
selection. GPUI `whitespace_normal`, a shrinkable text column, and auto-height
rows wrap at the available width; line numbers stay with their logical line.
Unwrapped mode keeps upstream horizontal scrolling. Transcript `CodeChunk`
now uses this instead of its plain-text fallback.

Anchors: `custom_elements/code.rs` → `show_line_numbers: bool`,
`for (line_ix, line)`, `if self.show_line_numbers`,
`content.child(row.child(ctx.text(...)))`, `.overflow_x_scroll()`,
`"showLineNumbers" =>`, and `prop_names`.
Implementation: `patches/code_wrap.rs`.
`code-wrap-check.ts`: a 300px block grows from 80px to 260px with soft wrap,
including Unicode and a long unbroken token.
The reviewed screenshot retains syntax colours and line numbers.

## Code editor textarea (ADE-61)

```tsx
<CodeEditorInput testId="file-editor" value={initial} theme={markdownTheme}
  syntaxDocument={doc.id} syntaxRevision={doc.revision} softWrap={wrap} showLineNumbers
  revealLine={`${line}:${nonce}`} interceptKeys={[...]} onChange={...} />   // src/native/code-editor.tsx
```

`<textarea codeEditor>` keeps the native edit model (selection, IME, clipboard,
word/line navigation, undo/redo) and adds what a file tab needs:

- Syntect colours from a registered syntax document (`createSyntaxDocument` /
  `updateSyntaxDocument`). Typing never waits for a parse: until the next
  revision lands, the last revision's spans are mapped onto the live buffer by
  common prefix and suffix, so only the edited span is plain.
- A line-number gutter per logical line (wrapped rows keep their number; the
  caret's line is `text_muted`, others `text_faint`), at least 44 px wide.
- `softWrap={false}`: no wrapping, horizontal wheel/trackpad scroll and caret
  follow. Default (absent or true) wraps at the text column's width.
- The editor fills its box and scrolls inside it. Measure never shapes text;
  prepaint shapes at the final width with a per-line cache (lines are moved
  over from the previous layout by a hash of text and runs), and an unchanged
  frame does no text work. Paint skips rows outside the viewport (all
  textareas).
- `revealLine="<1-based line>:<nonce>"` scrolls that line three rows from the
  top and puts the caret on it, once per new value.

Measured on a 5,000-line TypeScript file (dark, Reduce Motion, background
window, `KYBERN_FRAME_COSTS`, `src/dock/verify-editor.ts perf`): the window draw
after each of 30 keystrokes was 7.7–8.3 ms median, 25 ms p95, 37 ms max. The
stock textarea reshaped the whole buffer at several Taffy probe widths per
frame: 4.7 s median per keystroke on the same file.

Anchors: `custom_elements/input.rs` (see `patches/code_editor_hooks.py`).
Implementation: `patches/code_editor.rs`. Limits: no LSP, no multiple
cursors; `decorations` still work but syntax colours win their runs; the undo
history is the stock 200 snapshots of the whole buffer.

## Native springs and translation

```tsx
<motion.div initial={{ translateX: 0, translateY: 0 }}
  animate={{ translateX: 200, translateY: 20 }}
  transition={{ type: 'spring', duration: 0.25, bounce: 0.3 }} />
```

The existing Rust motion clock samples a closed-form damped spring. `duration`
is seconds, `bounce` is 0…1; zero is critically damped. This is an Apple-style
parameter API, not Apple's private curve. It settles exactly at the duration;
opacity and nonnegative dimensions are clamped. Retargeting preserves current
position but does not preserve velocity. Existing ease tweens remain unchanged.

Translation wraps a subtree in an `Element` that delegates layout and calls
`Window::with_element_offset` in prepaint/paint. Siblings do not reflow; hitboxes
move with paint. It is a motion property, not a general static style transform.

Anchors: `motion.rs` → `pub border_radius`, `fallback.border_radius`,
`target.border_radius`, `struct MotionTransition`, `duration: default_duration`,
`let progress = ease(...)`, `validate_ease(...)`, `("borderRadius", ...)`,
and the opacity/width/height/radius style assignments;
`renderer.rs` → `ctx.inherited = parent_inherited;
    built`.
`patch_react.py` adds translate values and spring parameters to motion identity.

`native-motion-check.ts` uses the paused native clock: a 200px translation
overshoots by 19.0px, ends at exactly 200×20px, leaves a sibling unchanged,
and remains clickable at its translated position. The spring unit check covers
finite output, endpoints and critical/underdamped behavior.

**Scale (historical note, superseded by `paintTransform` below).** This GPUI fork has transform matrices on glyph
sprites, not a subtree scene transform for quads, shadows, paths, surfaces,
clip masks and hitboxes together. Scaling only cached paint would leave these
inconsistent. A faithful implementation needs coordinated scene + clip + input
support in GPUI. Use translation with opacity, or layout-changing width/height
where that behavior is intended; neither is presented as scale.

## Virtual-list cache ownership

The previously suspected speculative-prepaint fault was a width-measurement bug.
A cached view deliberately has no intrinsic content width. An unfocused list row
returned its child directly, so percentage-sized islands in an auto-width row
resolved to zero. `build_virtual_child` now gives these rows the same full-width
container already used for focusable rows. No GPUI fork changes are required.
The native entity remains owned by the island registry, keyed by retained node ID;
changed subtree revisions notify it before drawing. The inline-first-frame rule
and row identities remain intact. `KYBERN_INLINE_LIST_ISLANDS=1` restores the
old inline path for diagnostic A/B measurements; it is off by default.

`native/capability-check.ts` verifies changed row text, stable nonzero width after
scrolling, and selection across replay in both appearances. The existing island
check compares the whole virtual list pixel-for-pixel with inline rendering.
Its old output label still says "inline fallback"; the current default does not
use that fallback. The registry is pruned when retained nodes unmount; callers
must continue to bound their React history window, not mount unlimited history.

## Verification commands

`update.sh` runs typecheck, the six original background-window capability scripts,
the input/selection/font/list check, layout/syntax checks, and the scratch full-shell
pointer check serially.
The physical pointer and external AX checks are separate because they require
OS permissions. `scripts/native-perf.ts` samples one full app window against an
explicit data directory and thread. Use the scratch daemon for writes; live
real-daemon measurements are read-only. Keep only one window per worktree.

The generated `native/build` (about 1.9 GiB) and `native/upstream` (186 MiB)
are retained for the integrating agent, as requested. Free disk went from
73 GiB before the first native build to 68 GiB at completion; other agents
were working concurrently, so the total change is not solely this build.

## Embedded web view on Linux (WP10)

`webview-linux.c` (`libkybern-webview.so`) is the Linux counterpart of `webview.m`: WPE WebKit renders offscreen and
the fork's `<websurface>` element (`custom_elements/websurface.rs`) paints the page as a texture inside the element
tree. Design, status and gaps: [`../WEBVIEW-PLATFORMS.md`](../WEBVIEW-PLATFORMS.md#linux-implementation-wp10-2026-10-26).

```sh
sudo apt-get install $(wpe/build-wpe.sh deps)    # ubuntu-22.04, the glibc floor the packages target
wpe/build-wpe.sh build/wpe                        # ~1.5 h on 4 cores; WPE_WORK, WPE_JOBS, WPE_KEEP_WORK
bun build-webview.ts                              # build/libkybern-webview.so (RUNPATH $ORIGIN/wpe/lib); also update.sh
xvfb-run -a bun webview-linux-check.ts            # the lab: rounded clip, popup over the page, input, <select>, snapshot
```

On a distro that packages WPE WebKit 2.0 (Debian sid) `build-webview.ts` links the system one instead, for
development. Pages are never run unsandboxed: without a working bubblewrap, inside a Snap environment or Flatpak, or
with `WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS` set, the web view is off (`webHost.unavailableReason === 'sandbox'`) and
every surface falls back to the browser (WP4); Kybern's WebKit aborts rather than start an unsandboxed web process
(`wpe/wpe-sandbox.patch`). There is no unsandboxed test mode: containers without user namespaces cannot run the web
view. `bun webview-linux-sandbox-check.ts` checks those refusals. `KYBERN_WEBVIEW_DEBUG=1` logs the companion's frame rate and
copy cost; `KYBERN_DEBUG_WEBSURFACE=1` the element's uploads.

## Running the checks on Linux and Windows (WP7)

A first set of the on-screen checks runs on all three OSes through one harness, `native/check-harness.ts`, and one
runner, `scripts/run-platform-checks.ts`. The checks drive the app and the labs through the addon's automation client
(`mouse.*`, `press`, `getPaintedText`, element bounds: input is synthesized inside the process, so no NSEvent, Swift or
xdotool) and capture with `capture(fixture, file)`:

| OS | Capture |
| --- | --- |
| macOS | the addon screenshot, as before; `{ webViews: true }` captures the real window (Swift window id + `screencapture -l`) to include an embedded web view |
| Windows | the addon screenshot (Direct3D 11; WARP in CI) |
| Linux | the addon screenshot (GPUI renders the scene and reads it back through wgpu; lavapipe in CI). An older addon falls back to the X server (`xwininfo -root -tree` picks the window, ImageMagick `import` reads it), which needs Xvfb started with `-extension Composite` (with Composite, GPUI's 32-bit window is redirected off-screen and captures black). `KYBERN_CHECK_CAPTURE=addon\|x11` forces one; the log names the choice |

| Check | Run by `--checks` | What it proves |
| --- | --- | --- |
| `native/focus-check.ts` | `focus` | Tab/Shift+Tab move one stop, a consumed Tab stays, the image preview traps Tab (no pixels) |
| `native/clip-check.ts` | `clip` | rounded clip hit tests and painted corners (dark/light, motion on/off) |
| `native/rounded-check.ts` | `rounded` | rounded input backgrounds, outset clamps, wrapped fragments, selection |
| `native/text-check.ts` | `text` | WP8: on a type specimen (`text-lab.tsx`), every kit weight of the UI font and of Inter paints wider than the one below it (the `wght` axis), line boxes, cap heights and baselines of Inter and JetBrains Mono within 1 px of macOS (`text-macos.json`, `--record` on a Mac), the code face advance, `kszl` inline code, colour emoji and CJK fallback; writes `text-<os>.png` and `text-metrics-<os>.json` (with DirectWrite's `KYBERN_FONT_DEBUG` lines) |
| `native/markdown-document-check.ts` | `markdown-document` | document API; document pixels equal the original Markdown path (same renderer, exact); selection, wrap, grid. Off macOS a subset of the interior cases (`MD_INTERIOR_CASES`) |
| `scripts/platform-smoke.ts` | `smoke` | the real app on a seeded daemon: sidebar rows, Markdown + code transcript, composer; the clip lab with rounded clip on and off (Linux/Windows) |
| `scripts/nowebview-app-check.ts` | `nowebview` | WP4: the app with the web view off: preview in the browser, visual, question, Mermaid, artifact |
| `native/notify-check.ts` | `notify` (Linux, Windows) | WP5: system notifications through the app's `notifications` module. Linux: against a test D-Bus notification server (`native/notify-server.py`, needs `dbus python3-gi`) under `dbus-run-session`: the `Notify` fields, `replaces_id` per thread, no second sound for a quick replacement, two quick posts as one notification, `CloseNotification` on dismiss, no stale `replaces_id` after a server restart, markup escaped only for a `body-markup` server, `ActionInvoked` reaching the app once per click, `unsupported` with no daemon. Windows: the toast API with the loose run's AUMID (`dev.kybern.gpuix.dev`), its `AppUserModelId` registry entry (and none for the installed id), the posts, the action-center history, a dismissal, a post after the action center was cleared. A click on a toast is checked by hand |
| `native/webview-win-check.ts` | `webview` (Windows only) | WP10: a WebView2 visual in a GPUIX window driven with real OS input (user32) and checked on captures of the composed window (`PrintWindow`, which includes the WebView2 visuals the addon screenshot never sees): stacking, rounded clip, transparency, menu cut-out, modal dim, bridge both ways, click, double click, wheel, typing, Ctrl shortcuts, Escape and outside-press handback, `kybern.answer.draft`, `present`/`probe`, `snapshot` with alpha, `snapshotRect`, `frame`, hide and destroy |
| `scripts/window-chrome-check.ts` | `window-chrome` | WP3: the real app's title bar per OS: the traffic-light room on macOS; Windows caption buttons driven with real input (place and size, red close hover, maximize/restore, double click, a drag that moves the window, a control in the bar keeping its click, minimize, close); Linux client-side decorations under xfwm4's compositor through `xdotool` (the same plus a right-edge resize; `KYBERN_EXPECT_DECORATIONS=client`); the Ctrl (⌘) shortcut key everywhere. Writes `titlebar*.png` crops and `window-chrome.json` |

Off macOS GPUI dispatches synthesized input on its UI thread and the app's handler runs on the JS thread after the
automation call returns, so a check reads what a click changed through `settled(read, done)`, not once.
Pixel assertions tolerate software rendering: `tolerance` (3 on macOS, 8 elsewhere, `KYBERN_CHECK_TOLERANCE`), regions
that must hold painted content (`assertPainted`), structure (bounds, test ids, painted text), and exact equality only
between two renders of the same renderer. Steps that need a capability still blocked off macOS are wrapped in
`step(label, fn, { gap })` with a `GAPS.*` reason (wheel input: scroll phase injection, WP1; ⌘ shortcuts, WP3; Menlo
advances, WP8, which reports closed since the labs use the bundled JetBrains Mono off macOS). Off macOS a gap failure is recorded and listed in the summary, not failed, until `--strict` /
`KYBERN_CHECK_STRICT=1`; a gap that passes is reported as closed so its marker can go.
`KYBERN_OPEN_LOG=<file>` makes `os.openTarget` append instead of opening, so a check can assert what the app opened on
any OS.

Locally (Linux needs `xvfb xauth x11-utils imagemagick` and Mesa's `mesa-vulkan-drivers`):

```sh
cd apps/gpuix && pnpm install --frozen-lockfile && (cd ../desktop && pnpm install --frozen-lockfile)
cargo build -p kybern                                    # the scratch daemon
native/update.sh                                         # or set NAPI_RS_NATIVE_LIBRARY_PATH to a built addon
xvfb-run -a -s '-screen 0 1440x900x24 -extension Composite' \
  bun scripts/run-platform-checks.ts --kybernd ../../target/debug/kybernd --out /tmp/gpuix-checks --checks clip,focus
bun scripts/run-platform-checks.ts --list                 # the check names
# A display nothing can grab (headless Weston): the smoke runs once without PNGs
bun scripts/run-platform-checks.ts --kybernd ../../target/debug/kybernd --checks frame-wake,smoke --capture none
```

Windows and macOS use the desktop session (no Xvfb). Any single check also runs by hand
(`KYBERN_VERIFY_OUTPUT=<dir> bun native/clip-check.ts`); with no `KYBERN_DATA_DIR` the harness uses a temp folder, never
`~/.kybern`. Output per check: PNGs and `log.txt` under `<out>/<check>/`, plus `report.jsonl`, `summary.md` and
`summary.json`.

CI: `.github/workflows/gpuix-platforms.yml` (manual: Actions → "GPUIX platforms" → Run workflow). Inputs: `checks`
(`all` or a comma list), `strict`, `wayland` (on by default: a headless-Weston job, no `wl_seat`, that runs `frame-wake` and the smoke with
`--capture none`, since nothing can grab that screen), `lto` (off builds the addon without fat LTO: enough to check a
patch, and cached under its own key). Jobs: `ubuntu-22.04` (Xvfb 1440x900 + lavapipe) and `windows-2022` (WARP, the desktop's
1024x768). Artifacts `gpuix-<id>-checks` hold the screenshots and logs; the run page shows `summary.md`.

## Extension performance (2026-10-15)

Apple M1, macOS 27.0 (26A428), Bun 1.4.0, GPUIX 0.10.0, 1280×820 background
window, 10-second warmup then a 15-second `perf/sample.ts` sample. Before is the
original islands binary (`build/baseline.node`); after is this extension build.
One window at a time; concurrent agents and varying occlusion limit precision.
GPU time is per-process Metal time, not GPU percentage or energy.

| Fixed full-app scene | Before CPU / footprint / GPU | After CPU / footprint / GPU |
|---|---|---|
| Settled scratch thread, no loader | 3.07% / 176 MiB / 8.1 ms | 3.20% / 180 MiB / 1.3 ms |
| Running fixture, two 5 Hz indicators, one live tool row | 8.27% / 174 MiB / 46.0 ms | 9.80% / 174 MiB / 54.3 ms |

The running fixture uses the actual app, sidebar and transcript virtual lists;
`native-perf-fixture.tsx` freezes scratch transcript data into a running state
in this process only. It makes no daemon writes. The 1.53-point CPU difference
is below the existing report's 3-point noise threshold; this is not evidence
of a speedup. Idle is close to the existing ~3% target, slightly above it.
No new continuous timer was added by the native capabilities.

For completeness, the changing live orchestrator thread (read-only real daemon,
no warmup) measured 14.87% / 182 MiB / 51.2 ms before and 18.87% / 180 MiB /
52.9 ms after. Its content changed between samples, so it is not a controlled
regression comparison. An initial scratch run became idle during measurement;
it was discarded as an active-workload comparison.

Reproduce the controlled scene with `KYBERN_DATA_DIR`, `KYBERN_GPUIX_THREAD`,
`NATIVE_PERF_FIXTURE=1 NATIVE_PERF_ACTIVE=1 NATIVE_PERF_WARMUP=10` and
`bun scripts/native-perf.ts <label>`. Omit the two fixture/active flags for idle.
Physical footprint is not RSS. Commit time, input latency and energy were not
measured. The optional selection-range API was not implemented.


## Pointer regression verification (2026-10-15)

`combined-pointer-check.ts` reproduces full-shell Settings pointer/Space input
against an explicit scratch `KYBERN_DATA_DIR`. Set `KYBERN_GPUIX_THREAD` to a
scratch thread to also check sidebar selection, titlebar dock toggle, completed
dock entrance, adding Notes, and collapse. `native-check.ts` additionally checks
an excluded titlebar button and a direct pointer click at y=47, one pixel below
the 46px drag region. These are GPUI automation events; OS event-posting and
physical window movement retain the manual-verification limitation above.

The fixed build passes all six scripts run by `update.sh`, including the island
pixel/hover/animation comparisons, and `pnpm typecheck`. `bun test ./src` passes
15 app tests / 54 assertions. Bare `bun test` also discovers the retained
`native/upstream` checkout: that unrelated suite fails on missing upstream
packages (including `goke`) and updater executable-symlink checks. No upstream
dependencies were installed to hide that distinction.

Controlled active-scene comparison on Apple M1, macOS 27.0, Bun 1.4.0,
GPUIX 0.10.0, 1280×820 background window: the full merged app with a scratch
thread, one user message, one live tool row, and two 5 Hz indicators through
`native-perf-fixture.tsx`. Each independent launch warmed for 10 seconds, then
sampled for 15 seconds. Before is the reproduced broken extensions build;
after changes only the missing-bounds replay guard. One fixture window at a time.

| Sample | CPU | Physical footprint | Process Metal GPU time |
|---|---:|---:|---:|
| Before 1 | 7.00% | 167 MiB | 81.2 ms |
| Before 2 | 9.20% | 169 MiB | 78.3 ms |
| After 1 | 8.73% | 171 MiB | 71.8 ms |
| After 2 | 9.33% | 186 MiB | 76.5 ms |

Mean CPU changed by +0.93 percentage points (8.10% → 9.03%), within the
existing 3-point noise threshold. GPU time is similar; this does not demonstrate
a speedup. Footprint was higher by 2–19 MiB across runs; two short samples do
not establish memory equivalence or a leak. Concurrent system work and window
occlusion limit precision. Commit timing, input latency, and energy were not
measured. The guard adds no timer, retained allocation, or cache entry.

Free disk was 73 GiB before and after. The requested incremental build directory
(1.9 GiB) and upstream checkout (186 MiB) remain for integration; temporary A/B
binaries and source copies, scratch data, daemon, and fixture windows were
removed/stopped. The signed, load-checked deliverable is
`build/gpuix-native.darwin-arm64.node`.

## Native API additions (package 1, 2026-10-15)

Import `nativeCapabilities` from `src/native/api.ts` and pass the renderer from
`useGpuixRequired()`. It checks the actual binary and throws a named capability
error for an older/stock module. These additions are macOS-only. No npm dependency
or lockfile change is needed; the interface is owned by this client.

```ts
const native = nativeCapabilities(renderer)
native.getInputSelection(id) // { start, end, reversed, composing }
native.setInputSelection(id, start, end, reversed)
native.editInput(id, { start, end, text, expectedValue })
native.getTextSelection() // [{ elementId, sub, start, end, text }]
native.setTextSelection(id, sub, start, end)
native.registerFont(Buffer.from(fontBytes))
```

All offsets are **UTF-16 code units**, matching JavaScript string slicing. Negative,
fractional, out-of-range and split-surrogate positions throw. IDs are mounted
native host IDs (input refs), not test IDs. Selection direction is represented by
`reversed`; `start <= end` always. A selection read exposes no password text.
Selecting an input does not focus it; call the existing `focusElement` when desired.
Unmounted editors throw rather than silently retaining a detached edit model.
The weak editor registry prunes dead entries on subsequent input renders.

`editInput` synchronously checks `expectedValue` against the current native buffer,
replaces that range, places the caret at the inserted text's end, emits the normal
`onChange`, and creates exactly one native undo step. It preserves earlier undo
history and clears redo. The controlled React `value` must adopt `onChange`; use
the existing native echo handling rather than replacing the whole prop to format
a selection. Explicit edits reject read-only, secure and composing editors.
This is not a rich-document model or a multi-edit transaction group. A real IME
session and application menu integration still need their own verification.

Prose ranges contain the full text of each run, so selected text is
`range.text.slice(range.start, range.end)`. `getSelectedText()` still provides the
joined copy text. `setTextSelection` selects one painted run; for a host `<text>`
it selects the `sub`th descendant run, while code/Markdown use their native run
sub-index. It throws when the run is not painted. Ranges retain their text snapshot
across virtualization; consumers should not treat IDs as persistent document IDs.
Ordinary cached paint now restores text layout, start regions, painted text and
highlight inspection in order. Selection changes use GPUI refresh so cached wash
paint cannot remain stale. Deferred overlay text remains outside this snapshot;
do not wrap selectable overlay content in a cached ancestor.

Register fonts **before first use of their family**: GPUI caches font resolution.
The existing `main.tsx` `Fonts` gate already loads both bundled TTFs before mounting
App, so this native method enables it without an App/TitleBar patch. The optional
`registerBrandFonts(renderer)` helper performs that registration once per renderer.
Invalid font data propagates the native error. Registration is process-local;
it does not install fonts into macOS. The fixture changes from Helvetica to
Cal Sans and asserts different shaped glyph metrics.

`native/capability-check.ts` runs dark and light windows and checks Unicode ranges,
selection direction, native undo and redo, stale-edit rejection, font metrics,
prose range retention through three unrelated frames, changed virtual row text,
and scrolling to a previously unseen cached row. Run it after the existing six
capability scripts. `pnpm exec tsc -p native/tsconfig.json` checks these fixtures
without including the disposable upstream checkout.

## Frame scheduling and subtree scale

The pinned GPUI already implements `Window::request_animation_frame()` through
`on_next_frame`/`schedule_frame`, with `gpui_macos/src/display_link.rs` supplying
CVDisplayLink pacing and idle suspension. Native motion uses this path. The shared
The shared source now supports display-rate JS sampling and native matrix/task
loops without React commits. Since motion/fps (2026-10-20) native loops are the
default with the patched addon (`KYBERN_NATIVE_PAINT_ANIMATIONS=0` /
`KYBERN_NATIVE_TEXT_PAINT=0` select the old 5 Hz React clock and React-sampled
live labels for comparisons), and settled loops run in Core Animation
("Compositor loops" below). Elapsed labels have a
separate visibility-aware seconds clock. Round-seven measurements and remaining
acceptance gaps are in `../perf/pacing-round7-2026-10-17.md`.

**Frame watchdog (macOS, fork branches `frame-stall`).** A window draws only when its display's link calls
`step`, and AppKit re-subscribes it only on occlusion, screen and key changes. A link that stops calling back while
the registry believes it runs (CoreVideo, seen on Apple silicon), a window left unsubscribed, or one subscribed to a
display it left froze the whole UI until it was minimized, changed Space, moved to another display or became key
again; a move on the same display does not help. `pump_events` (at least every 100 ms through the JS fallback timer,
no timer of its own) checks each visible window: 250 ms without a frame re-subscribes it, restarts a silent link and
then replaces it (the old one leaked, as every link is). Screen-parameter and wake notifications re-subscribe at once.
A draw whose `nextDrawable` timed out (1 s, all drawables held by the compositor) presents the same scene again after
100 ms, backing off to 1.6 s, instead of leaving the previous frame up until something else changes.
`getFrameWatchdog()` reports frames, stalls, repairs, the last stall in words, and dropped/slow drawables; a stall or
a first dropped drawable raises a window-state event, and `src/platform/frameWatchdog.ts` appends a line to
`gpuix-frames.log` in the data dir. `KYBERN_FRAME_WATCHDOG=0` turns it off, `=log` only reports.
`KYBERN_FRAME_STALL_TEST=silent-link|dead-link|unsubscribe|drop-drawable:<ms>` injects each fault;
`frame-stall-check.ts` (with `frame-stall-lab.tsx`) shows each one freezing with `=log` and repaired with the
watchdog on. Measurements: `../perf/frame-stall-2026-10-29.md`.

**Drawable starvation (macOS, fork branches `drawable-starvation`).** The freeze sampled in the field (0.17.0) was
none of those states: the link ran, and the main thread sat in `-[CAMetalLayer nextDrawable]` for every frame. When
WindowServer shows none of the drawables a layer presented, `nextDrawable` waits about 1 s and then hands one back
anyway (`allowsNextDrawableTimeout` is NO, so it never returns nil and the dropped-drawable path above never ran),
and a window drawing on every frame spends the whole time there: no input, no JS, nothing new on screen. The gate
in `gpui_apple/src/drawable_gate.rs` counts presented drawables until their presented handler runs. With two
unshown (the pool of three is empty) a draw waits for a handler at most until the older one is 100 ms old, then
skips the draw (`take_dropped_frame`, so the window presents the scene again later) instead of blocking. A
`nextDrawable` over 100 ms twice in a row starts the same episode. While starved the layer is kicked (pending
`CATransaction`s flushed, `presentsWithTransaction` off, `drawableSize` nudged for fresh surfaces) at 100 ms,
backing off to 3.2 s, and a draw is tried again as soon as a handler shows the compositor moving or after that
backoff. Presents with a transaction are not counted: their handlers run on the main thread. `getFrameWatchdog()`
adds `drawableStarvations`, `drawableRecoveries`, `starvedSkips`, `drawableKicks` and `lastStarvation`; an episode
starting and ending each raise a window-state event and a `gpuix-frames.log` line (`starvation x1 …`,
`starvation ended …`, with how long it lasted and what cleared it). `KYBERN_DRAWABLE_GATE=0` turns the gate off;
`KYBERN_FRAME_STALL_TEST=starve-drawables:<ms>` emulates a compositor that shows nothing for 3 s after every <ms>
(handlers held back, `nextDrawable` blocking 1 s once the pool is empty), and `frame-stall-check.ts` runs it with
the gate off and on. Whether the kicks release a real stalled compositor is unproven; the log says which cleared
it.

**Drawable acquisition (2026-10-30).** The gate cannot predict every slow `nextDrawable`: field logs on 0.19.4 still
record waits above one second with the gate enabled. The fault replay also reproduces a slow acquisition below the
gate's tracked-queue limit. Metal acquisition now runs on
one worker per window renderer (`gpui_apple/src/drawable_acquisition*.rs`), with one pending request/result and at most
one prefetched drawable. The input thread waits at most 1 ms for it. Until it arrives, the existing base frame and
compositor loops stay together; the latest scene is rendered when the drawable is ready. Readiness requests a present
on the existing display callback, bypassing the dropped-frame backoff without creating an idle render loop. A resize
discards an old-sized drawable. Closing a window does not join a worker blocked inside Metal. Headless renders use
their existing path. `KYBERN_ASYNC_DRAWABLE=0` selects the synchronous control; the asynchronous path also enables
Metal's drawable timeout. The gate still reports/kicks starvation, without waiting on the UI thread.

`KYBERN_FRAME_STALL_TEST=slow-drawable:<ms>` delays acquisition by one second regardless of the presented queue's
count. `native/update-freeze-check.ts` drives marquee rows, native compositor loops, streaming text, clicks and the
real composer editor, measures input and JS timer delay, and checks the exact click/character counts encoded in
submitted pixels after acquisition recovers. It reads presented-frame dumps without requesting a fresh screenshot.
This reproduces a confirmed blocking path; the original field trigger and any separate persistent stale-view fault
remain unproven until verified in normal use.

**Kick deadlock (ADE-110, fork branches `kick-deadlock`).** 0.20.4 froze with a spinning cursor: the main thread
waited forever on its own window lock. `MacWindow::draw` holds the window lock across `renderer.draw`; a starved draw
kicked the layer, and the kick's `[CATransaction flush]` ran AppKit's display cycle right there, whose tracking-area
update called `resetCursorRects`, which locks the same `parking_lot` mutex. The kick now flushes on a later turn of
the main queue (one flush pending at most), outside any draw; its other steps, counters and backoff are unchanged.
The AppKit callbacks a display cycle can run (`resetCursorRects`, `_opaqueRectForWindowMoveWhenInTitlebar`,
`displayLayer:`, `setFrameSize:`, `viewDidChangeBackingProperties`, `viewDidChangeEffectiveAppearance`,
`windowDidResize:`) no longer wait for that lock: when it is held they defer their work to the next main-queue turn
(the cursor rects are invalidated again, the frame size is applied to the renderer then), and the titlebar rect comes
from a view ivar. `makeBackingLayer` still locks; AppKit asks for it only when the view becomes layer-backed. Input
and drag callbacks still lock too: AppKit delivers them as events, not from a commit.
`KYBERN_FRAME_STALL_TEST=kick-reentry:<ms>` invalidates the cursor rects inside a draw and kicks every <ms>;
`flush-in-draw:<ms>` flushes inside the draw directly. AppKit updates cursor rects in a flush only for the key window,
so under either fault the background test window answers `isKeyWindow` with YES instead of activating the app.
`native/kick-reentry-check.ts` (with `frame-stall-lab.tsx`) runs both: before the fix each hung on the first fault
with `reset_cursor_rects` in `lock_slow`, the field stack; after it every fault's draw continues and the deferred
cursor rects are applied on the next turn.

Subtree scale is now `paintTransform` ("Paint-only subtree transforms" below); the reasons in the native springs
section: quads, paths, masks, glyphs and hitboxes need one coordinated GPUI scene
transform. Translation is not presented as scale.

## Embedded browser host (round 5)

`webview.m` is a small AppKit/WebKit companion library, built with
`bun native/build-webview.ts`. It does not rebuild GPUI or replace its frame loop.
`native/update.sh` builds it too; `scripts/package-app.ts` stages and signs it.
The runtime loads `native/build/kybern-webview.dylib`, or the app Resources copy.

`src/native/webview.ts` owns the trusted wrapper. Each WKWebView uses a fresh
nonpersistent website store and a native generation. A WKURLSchemeHandler serves
only that document's wrapper at `tauri://localhost/embedded/...`; this origin is
already allowed by the daemon's file `frame-ancestors` policy. File and visual
children remain opaque sandboxed iframes; server children use the desktop's
server sandbox. Native navigation action and response policies restrict the
child to its ticket/relay origin and forbid top-level replacement and popups.
No bearer token is sent to a page. Only main-frame wrapper messages enter the
public message bridge; a separate isolated WKContentWorld retains the direct
child's frame for back/forward/reload/stop. Untrusted child script cannot use it.

Windows (WP10) has the same host in `native/webview-win` (Rust, `kybern-webview.dll`, built by
`native/build-webview.ts` and `update.sh` on Windows): WebView2 `CompositionController`s under a
Windows.UI.Composition target above GPUI's swap chain, the same three C exports and JSON channel,
so `webview.ts` only picks the file. Its design and parity table are in `../WEBVIEW-PLATFORMS.md`.

`src/preview/relay.ts` mirrors the Tauri relay's HTTP request-head rewrite and
raw WebSocket splice. A loopback-only listener checks Host, refuses CONNECT and
absolute targets, and only targets a ticket on the configured daemon. Page
cookies/Authorization pass through; Kybern credentials are never added. The
existing daemon proxy still enforces its credential filtering and destination
checks. Closing a document destroys its relay sockets and revokes its ticket.

`src/preview/Surface.tsx` owns at most two preview documents (the selected thread
and one floating thread). Dock and mini-player publish clipped native geometry;
the WK view never moves between parents or reloads on a slot change. Hidden
docked documents unload after five minutes, with a new ticket on return. Native
window occlusion/minimize notifications pause visual bootstrap loops and suspend
media. Destruction removes both message handlers, observers, views, media, and
the last host's key and mouse monitors. Native web input routes app shortcuts
back to the existing JS shortcut handlers.

Keyboard handback (ADE-69): a clicked document becomes AppKit's first responder,
and GPUIView, its sibling, never reclaims it on click, so keys meant for the
composer went to the page and beeped. The document gives the keyboard back to
GPUIView on a press outside it (local mouse monitor), on an Escape the page left
unhandled (WebKit sends it up the chain as `cancelOperation:` to `KybernWebClip`;
preventDefault, a modal dialog or fullscreen keep it, and GPUI never sees it), on
hide or destroy, and on the `release` op, which `src/native/focusHandback.ts` sends
before every GPUIX focus move and on element focus events. Other unhandled plain
keys end silently at the clip instead of NSBeep. `webview-check.ts` covers it.

GPUI popups publish rounded occlusion paths, including a native dim layer for
modal scrims. An expanded visual inherits its popup layer so it appears above
that modal and preserves the same document. The existing composer measurement
observer also masks the native document below the floating composer. Ordinary
layout measurements do not add a polling loop.

Verification scripts: `native/webview-check.ts` exercises native AppKit pointer,
text input, Command-L, blocked external navigation, identity, visibility and
popup masking. `scripts/preview-app-check.ts` uses the scratch daemon for HTML,
localhost root assets, visual expansion, mini-player and device rotation in
light/dark and normal/reduced motion. Both acquire the round-5 UI lock and take
window-only `screencapture` images: GPUI's own screenshot API excludes child
NSViews. See the round-5 preview report for actual results and remaining gaps.

## Event-driven layout (package 2)

```ts
import { onWindowResize, observeElementBounds } from '../native/layout.ts'
const stopSize = onWindowResize(renderer, ({ width, height, scaleFactor, frame }) => {})
const stopBounds = observeElementBounds(renderer, { testId: 'right-dock' }, event => {})
// target also accepts a numeric host ID or a PublicInstance { id }.
// event: { elementId: number | null, rect, clippedRect, frame }
stopBounds() // idempotent, suppresses already queued JS deliveries
```

`LayoutRect` is `{ x, y, width, height }` in logical content-area pixels, including
native translation. Each observer receives its initial value after paint, then
only changed values, at most once per completed observed frame. A zero-size root
canvas defers comparison until ordinary paint, deferred overlays and cached replay
have finished. Callback-triggered updates therefore render in a subsequent frame.
The frame counter counts observed paints, not compositor presentations. There is
no new timer, polling read, cache invalidation or animation-frame request.

`clippedRect` intersects the box with its effective native content mask and viewport;
fully clipped boxes have a null clipped rect. `rect: null` denotes an unpainted
virtual row or removed host. Cache snapshots preserve this clipping information.
Test IDs must initially resolve uniquely. They follow a replacement host; if later
absent or ambiguous, they produce null until unique again. Numeric IDs remain tied
to that host, returning null after unmount. Unsubscribe is safe after window close;
queued JS callbacks check native subscription liveness before invoking app code.
The view drops all subscriptions on close. At most 256 observers may be active.
Off macOS `addLayoutObserver` returns its token without waiting for the UI thread
(the add is queued in order with the other commands), so a refused target arrives
as the callback's error instead of a throw; macOS still throws.

`layout-hooks.ts` exposes `useNativeWindowSize()` and `useElementBounds(testId)`.
The window hook performs one initial size read and then subscribes. Resize and
scale use the native painted viewport; minimize/occlusion are not synthetic resize
signals. Real cross-display scale changes remain unverified on this single-display
fixture; retain the separate visibility signal for animation suspension.

`native/layout-check.ts` checks dark/light initial values, native zoom resize,
effective clipping (including a changed ancestor clip), unchanged cached replay,
native translation, changed child coordinates, test-ID
replacement, numeric unmount, virtual unpaint, duplicate rejection, rapid/idempotent
unsubscribe and per-frame delivery identity. It also checks spring initial velocity.

## Whole-document syntax and virtual code (package 2)

```tsx
import { createSyntaxDocument, updateSyntaxDocument, releaseSyntaxDocument,
  getSyntaxDocumentText, VirtualCode } from '../native/syntax.tsx'
const doc = await createSyntaxDocument(renderer, { text, language: 'typescript', path })
// { id: string, revision: number, lineCount: number }
const next = await updateSyntaxDocument(renderer, doc.id, {
  expectedRevision: doc.revision, text: editedText,
})
<VirtualCode document={next.id} revision={next.revision}
  startLine={64} endLine={128} showLineNumbers softWrap />
getSyntaxDocumentText(renderer, next) // exact full source for Copy all/export
releaseSyntaxDocument(renderer, next.id)
```

`VirtualCode` validates integer, nonnegative, non-reversed ranges and renders the
native `<code>` document props. `endLine` is exclusive. The native adapter clamps
valid ranges to the document's line count; use the typed wrapper rather than
bypassing its validation with raw host props. Line numbers are absolute.

One immutable native source, line-offset index and whole-document neutral syntax
result serve every visible range. Parsing runs on the napi worker pool, never on
the paint thread. No range starts its own parser. Theme changes recolour the shared
spans without parsing. Mounted ranges hold an Arc lease; unmount releases it.
Updating reserves the next revision immediately: stale expected revisions reject.
A superseded/released/closed parse cannot publish a result. Work that already entered
Syntect finishes in its worker and is discarded; it is not forcibly interrupted.
The previous ready revision remains available during an update. App code should
adopt the returned handle only after its promise resolves.

Limits: 32 registered documents, 8 pending jobs, 2 MiB per source, 16 MiB of current
registered source, and 200,000 highlighted spans per document. Unknown grammars or
a span-limit/parser failure render exact plain source. Queued source copies, an
update's previous ready revision and mounted old leases are additional bounded-by-
caller storage; the 16 MiB limit is not a total-process-memory claim. Close/release
remove registry ownership and reject pending results. The app must still bound
mounted virtual rows and release handles when closing a file or environment.

Selection keys use a document-revision identity plus the absolute logical line,
so adjacent ranges and remounted ranges preserve selection and copy newlines once.
The existing selection API reports that stable document identity in `elementId`
and the absolute line in `sub`; it is not the transient `<code>` host ID. Offsets
within each selected run remain UTF-16. Source indexing is native UTF-8 internally.
Overlapping ranges should not be mounted twice in one document view. Wrapped row
height caching remains the viewport's responsibility; fixed-height Explorer rows
must not opt into wrapping without measuring their real heights.

Checks: `syntax-check.ts` compares PNG bytes to a single full-document render at
every line boundary for TypeScript multiline comments/strings, Python triple
strings, Bash heredocs, embedded HTML and unknown-grammar plain text, in dark/light.
`syntax-lifecycle-check.ts` checks exact cross-range Unicode copy, remount identity,
full-source retrieval, edits before the viewport, stale revisions/results, document
and queue bounds and release. `syntax-large-check.ts` mounts four rows from a
70,003-line, >512 KiB source and checks absolute numbers and bounded painted runs.

## Native spring initial velocity (package 2)

`transition={{ type: 'spring', duration: 0.32, bounce: 0.12,
velocityX: pointerPixelsPerSecondX, velocityY: pointerPixelsPerSecondY }}` adds the
homogeneous damped-spring response for each translation axis. Units are logical
pixels/second, not a normalized velocity, so a flick works even when source and
target are equal. Values must be finite and within ±1,000,000 px/s. Omitted axes
start at rest; this does not infer the velocity of a prior animation. Duration
zero still snaps exactly. Tweens ignore velocity. React motion identity includes
both fields, so a velocity-only change is forwarded. See `src/native/motion.ts`.

Paused-clock dark/light tests verify both axes, direction, same-target motion,
velocity-only retargeting and exact settlement. The standalone Rust spring tests
check the initial derivative, finite output, endpoints and damping behavior.

## Notifications and packaging (package 2)

OS notifications remain **blocked for the unbundled Bun host**. Do not call
`UNUserNotificationCenter.currentNotificationCenter` from that process: on this
macOS build it aborts inside `dispatch_once` with `bundleProxyForCurrentProcess is
nil`; an Objective-C `@catch` does not recover. Bun reports a C++ exception panic.
The deprecated `NSUserNotificationCenter.defaultUserNotificationCenter` returns
nil. The pinned GPUI `zed/crates/gpui_macos/src/system_notifications.rs` independently
guards its notification center with the same missing-bundle-ID check. `native/notification-check.ts` reproduces both in separate child processes,
then shows that the same native executable placed in a disposable `.app` with
`CFBundleIdentifier`, `CFBundleExecutable` and package type `APPL` can create the
modern center. The check cleans up its temporary bundle. **No authorization was
requested and no notification was posted.**

The minimal next step is an actual bundled native host or resident helper `.app`
with a stable bundle ID, a retained `UNUserNotificationCenterDelegate`, and an IPC
bridge carrying notification IDs back to the active environment/thread. The helper
must own status, permission requests, posting, and click delivery across activation
and launch. The probe establishes center creation only, not permission/display/click
behavior. It does not establish that a production signed helper is necessary for
local development; distribution signing belongs to packaging verification.

`src/native/notifications.ts` exposes a truthful unsupported status and explicit
errors for `requestPermission`, `post({id,title,body})` and `onClick`. It never
requests permission or silently substitutes in-app toasts. Keep the existing
agent toast flow until the bundled bridge has been verified. Apple references:
[UNUserNotificationCenter](https://developer.apple.com/documentation/usernotifications/unusernotificationcenter),
[authorization](https://developer.apple.com/documentation/usernotifications/unusernotificationcenter/requestauthorization(options:completionhandler:)),
[response delegate](https://developer.apple.com/documentation/usernotifications/unusernotificationcenterdelegate/usernotificationcenter(_:didreceive:withcompletionhandler:)).

Package 2's integration patch and final measurements are in [PACKAGE-2.md](PACKAGE-2.md).

## Package 1 validation and measurements

Full-shell `combined-pointer-check.ts` passes Settings pointer and Space, sidebar
thread selection, titlebar dock toggle, completed dock entrance, Notes addition
and collapse. `settings-check.ts` passes dark and light: switch RPC/event adoption,
numeric clamp/save, every section, search, Escape, shortcut and Back. Neither run
reproduced the old `Window move completed without beginning` warning. That does
not certify a physical AppKit drag: synthetic events do not supply a real NSEvent,
and the OS-authorized physical gesture check remains outstanding. No speculative
drag-hook change was made.

All six existing native checks pass after the final native rebuild; the new
dark/light capability check also passes. Application typecheck, native-fixture
typecheck and `bun test ./src` pass (15 tests, 54 assertions). The build is signed
and load-checked at `native/build/gpuix-native.darwin-arm64.node`.

Apple M1, macOS 27.0, Bun 1.4.0, GPUIX 0.10.0, background windows, 10-second warmup
and 15-second sample per independent launch. CPU is process CPU-time delta,
memory is `footprint` physical footprint, GPU is process Metal accumulated time.
Concurrent workers and occlusion caused substantial run-to-run variation; these
are not exclusive-machine benchmarks, energy or frame-latency measurements.

| Running full-app fixture | CPU | Footprint | GPU / 15 s |
| --- | ---: | ---: | ---: |
| Before 1 | 7.53% | 176 MiB | 75.1 ms |
| Before 2 | 4.67% | 174 MiB | 72.7 ms |
| After 1 | 9.33% | 172 MiB | 75.0 ms |
| After 2 | 6.00% | 179 MiB | 61.5 ms |

The existing deterministic running-thread fixture has one live tool row and two
5 Hz indicators, not sustained token streaming. Mean CPU is +1.57 points, within
the prior report's 3-point noise threshold; mean footprint is +0.5 MiB. This does
not establish a speedup or prove exact equivalence. The fixture mounts App directly
and does not exercise startup font registration; the full-shell Settings checks do.

The isolated `native/list-perf.ts` A/B uses the **same final binary**, 300 virtual
rows and one 10 Hz row loader (plus its outside reference loader), toggling only
the diagnostic inline-row path. This avoids changes in other native capabilities.

| Virtual-list fixture | CPU | Footprint | GPU / 15 s |
| --- | ---: | ---: | ---: |
| Inline 1 | 16.13% | 171 MiB | 102.4 ms |
| Inline 2 | 9.33% | 171 MiB | 71.0 ms |
| Cached 1 | 11.73% | 170 MiB | 91.3 ms |
| Cached 2 | 13.00% | 172 MiB | 92.8 ms |

Mean CPU is 12.73% inline versus 12.37% cached, footprint is 171 MiB for both;
GPU is 86.7 versus 92.1 ms. Variation dominates the small mean differences. The
rendering correctness fix is established; a meaningful CPU saving is not.

Affected issue #108 statuses:

| Line | Status | Evidence / remaining work |
| --- | --- | --- |
| 1.9 | Done | Full-shell dock toggle/entrance passes. |
| 1.10 | Partial | Native drag hook retained; warning not reproduced, physical gesture unverified. |
| 9.8 | Partial | Input selection/atomic undo API available; rich tokens and real IME integration belong to composer. |
| 16.6 | Partial | Selection formatting transactions available; Notes UI must consume them. |
| 22.1, 22.3 | Done | Full Settings dark/light interaction and RPC/event checks pass. |
| 24.15 | Partial | Unicode transaction undo/redo passes; rich editor/IME/menu integration still pending. |
| 25.4 | Done | Shared loader clock preserved; list-row caching now works. |
| 26.6 | Done | Native registration loads both bundled fonts through existing startup gate. |
| 28.11 | Done | Virtual rows, cached updates and newly scrolled rows pass. |

No App.tsx or TitleBar.tsx patch is required. Embedded WKWebView, event-driven
layout and virtual whole-document syntax are proposals only. Native notifications
were not part of the implementation brief handled here and were not added.

Free workspace-volume space was 73 GiB before and 72 GiB after, with other workers
active. The requested pre-existing incremental build (1.9 GiB), upstream checkout
(186 MiB), and reused dependencies (283 MiB) remain for integration. No second
build directory or daemon build was created. Scratch fixtures remove their daemon
data and close their own windows; no task process remained at handoff. Final
screenshots/logs are in `/Users/mymac/projects/ade/previews/gpuix/p1/`; redundant
task screenshots were removed from the native build directory. The installed app,
port 4219 and normal user data were not changed.

## Exact native diff scroll anchors

Scrolling `<diff>` elements publish their own live `gpui::ListState` through the
renderer’s existing virtual-list registry. `getListScrollTop(diffId)` returns
`[itemIndex, offsetInItemPx, viewportHeightPx]`, and post-paint bounds observers
include `scrollTop: [itemIndex, offsetInItemPx]`. These are GPUI’s logical row and
pixel offset, including fractional wheel movement. `scrollToItem(diffId, index,
offsetInItemPx)` uses the existing queued-scroll and cached-ancestor invalidation
path; the custom adapter consumes the request after rebuilding row measurements.
No new binding or scroll approximation is involved.

The custom registry owns the handle’s lifetime, including cached renders, empty
patches, flow mode, unmounts and reused host IDs. Separate diff elements and
ordinary virtual lists keep independent states. `native/diff-scroll-check.ts`
checks real wheel/read/remount/restore, observer anchors, identical restored
viewport pixels, nested cached islands, concurrent diffs, ordinary lists,
patch rebuild/shrink and lifecycle cleanup. Run it from `apps/gpuix` with the
current local addon: `bun native/diff-scroll-check.ts` (the harness uses scratch
client data and closes its window).

## Cached scroll requests and decorated inputs (package 3)

`renderer.scrollToItem(elementId, index, offsetInItem?)` now wakes every cached
ancestor of a pending virtual-list scroll before invalidation. Pending offsets
survive root element construction until cached child views have built; unused
requests are discarded by a deferred callback after the draw. Child-splice
ordering stays unchanged. The regression nests a **48-row** virtual list under
`cache="fill"` and a second cached island: its logical top changes from `[0, …]`
to `[35, …]` and remains there through an unrelated update. The TaskWindow effect's
manual `setRange({start: …, end: …})` pre-mount workaround can be removed; keep its
normal `onVisibleRange` overscan and focused-row mounting. A declaration is in
`src/native/api.ts` (`CachedListCapabilities`).

```ts
import { decoratedInputCapabilities, type InputDecoration } from '../native/api.ts'
const api = decoratedInputCapabilities(renderer)
api.setInputDecorations(inputId, [
  { start: 6, end: 15, id: 'computer', atomic: true,
    style: { color: '#b5d8ff', backgroundColor: '#244263', fontWeight: 600 } },
], 'Hello @Computer') // expectedValue is required: stale effects reject
api.hitTestInputDecoration(inputId, windowX, windowY) // string | null
```

The APIs are native renderer methods; the TS helper checks availability. All
external ranges are ordered UTF-16 character boundaries (surrogate splits reject).
`setInputDecorations(id, decorations, expectedValue)` replaces the entire list,
including clearing with `[]`, without changing text or adding an undo step. It
requires the exact current buffer. Unmounted/secure/composing editors reject.
Read-only inputs can be decorated. Validation is all-or-nothing: nonempty ranges,
at most 4096 spans, unique nonempty IDs when supplied, finite weights 100–900,
valid native colors, `fontStyle: 'normal' | 'italic'`, and nonoverlapping atomic
spans. Ordinary styling spans may overlap; later explicitly specified properties
win. Identical updates do nothing and decoration updates preserve typing's undo
coalescing.

Supported appearance: color, background, font weight/style, and underline.
Backgrounds paint before selection and glyphs using an input-local painter over
GPUI's existing wrapped geometry. `borderRadius` accepts finite nonnegative pixel
values; `backgroundOutsetX` is a finite nonnegative visual-only horizontal outset.
It is capped to half the adjacent undecorated whitespace and the input edges.
Adjacent same-color spans retain separate corners. Wrapped fragments round only
the span's external ends. `paddingX` still accepts only zero; nonzero values throw. This is styled source text, not arbitrary inline React views, replacement
labels, hidden Markdown syntax, a monospace-family override, or layout-padded
chips. Atomic means editing as one unit; it does not make a long token unbreakable
when wrapping. The actual buffer, copy text, accessibility text and IME text remain
identical; no placeholder characters or separate hidden document are introduced.

Caret navigation and mouse placement cannot leave the caret inside an atom.
Movement snaps in the direction of travel; `setInputSelection` snaps an interior
collapsed position to the end and expands partial nonempty selections outward.
Backspace, forward delete, cut, typing over a selection and `editInput` replacement
expand intersected atomic ranges. A native edit shifts untouched spans, discards
spans intersected by the edit, and retains IDs. Insertion at either boundary stays
outside the span. Undo/redo snapshots contain text, selection and decorations, so
deleted atoms return as atoms. External replacement through the `value` prop clears
decorations together with the upstream undo reset; normal `onChange` echoes retain
both. Apps should derive formatting again after ordinary text edits when desired.

IME: explicit decoration/selection/edit APIs reject while a marked range exists;
atomic snapping is bypassed during composition. Initial composition replacement
expands an intersected atom before editing, then the native marked-range pipeline
owns subsequent replacements, selection and commit. Its underline takes priority.
No key interception, text substitution or new composition callback is introduced.
A real input-method candidate/commit/cancel session remains a manual integration
check; automated key tests do not certify OS IME behavior.

`hitTestInputDecoration` takes logical window content coordinates, checks the
editor's content bounds, uses the native nearest-character hit test (including
scroll/wrap geometry), and returns the last matching identified span. Outside the
input or without an identified range it returns null. It is a query; it does not
activate a reference or suppress native caret behavior. A caller must decide which
pointer gesture should open a link.

`DecoratedTextarea` in `src/native/decorated-input.tsx` preserves ordinary textarea
props and refs, subscribes to post-paint bounds for mount/remount readiness, and
applies current decorations after changes and key-up. Stale/composition updates
wait for the next edit/key-up; there is no timer. `input-marks.ts` supplies a known-
reference token adapter and a deliberately limited source-mode Markdown adapter
(single-line backticks, `**bold**`, `*italic*`, simple links, atomic `kybern://task/`
links). It skips code/escaped delimiters; it is not a CommonMark or rich-document
parser. Existing editors remain responsible for their storage/serialization.

`native/decoration-check.ts` runs dark/light and verifies styled runs, the mounted
React adapter, ID hit testing, invalid/stale requests, atomic caret jumps,
deletion, partial transaction replacement, Unicode rebase, native undo/redo and
the nested cached-list regression. Screenshots show the actual background,
weight, italic and underline. `update.sh` runs it with all prior capability checks.
`app-integration-3.patch` is an **unapplied** patch for Composer, Notes, task
descriptions and TaskWindow; see `PACKAGE-3.md` for evidence and limits.


## Rounded input backgrounds (package 4)

See `PACKAGE-4.md` for the exact Tauri style mapping, reproducible lab, performance
samples and unapplied integration patch. `native/rounded-check.ts` verifies actual
pixels in dark/light: radius, adjacent same-color identity, outset clamping at
neighbours/input edges, square continuation ends, selection over a pill and atom
caret movement. `update.sh` includes it. No Zed/shared `TextRun` patch is needed.
The painter does not alter glyph advances, wrapping, selection, hit tests or IME.

```ts
style: { backgroundColor: '#28517c', borderRadius: 5, backgroundOutsetX: 4 }
```

Outset borrows existing whitespace; it does not create padding. Zero is the
integration default. Rounded backgrounds are used only for Notes/task-description
inline code. Tauri composer reference chips and resting task atoms have no fill:
`referenceDecorations` now uses weight 500 and no background; pass `t.accentBlue`.
`noteInputMarkColors(t)` resolves Notes' foreground/transparent mixes (4% light,
5% dark code fill; 60% task text). Native source-mode editors still show Markdown
syntax and cannot reproduce Tauri's icons, replacement labels or per-run font size.


## Markdown style controls (package 5)

`theme.markdown` adds optional table grids, cell padding, quote paint/spacing,
list gutters/markers, heading rhythm, fenced-code wrapping, and mixed-size padded
inline-code boxes. Import `withMarkdownStyle`, `transcriptMarkdownStyle` or
`notesMarkdownStyle` from `src/native/markdown.ts`; the structural helper carries
the new fields through the existing theme prop. Importing the adapter also
augments `GpuixTheme` with the optional field; no npm declarations are replaced.
The native renderer reads them from its normal theme override, with no extra bridge.

```tsx
<markdown source={source} theme={withMarkdownStyle(theme, {
  tableGrid: true, tablePaddingX: 7.2, tablePaddingY: 5.6,
  tableBorder: border, quoteBorder: border, quoteBackground: 'transparent',
  quoteText: muted, quotePaddingLeft: 12.8, quotePaddingY: 0,
  inlineCodePaddingX: 5.6, inlineCodePaddingY: 1.6,
  inlineCodeSizeRatio: 13 / 14, codeWrap: wrap,
})} />
```

Inline radius and fill remain `metrics.mdInlineCodeRadius` and `codeWash`.
`inlineCodePaddingX` reserves real width; it is unrelated to the input decoration
API's zero-only `paddingX`. The mixed-font flow measures source ranges first,
creates GPUI fragments only inside the viewport, and retains original bytes,
links, paragraph-wide search and one selection group. Fragment identities use
source offsets, so reflow retains selection. No synthetic padding characters,
per-frame timer, new parser, highlighting worker or global cache was added.

For mixed-direction paragraphs the renderer deliberately retains its original
whole-paragraph shaping path; padded/smaller code is not yet supported there.
Whitespace-containing code spans use an inline box (wrapping within that box when
wider than the viewport), not WebKit's full inline-fragment line-breaking algorithm.
Neither limitation should be described as full CSS layout parity.

`native/markdown-check.ts` covers dark/light pixels, Unicode selection, paragraph
search, reflow, hard breaks, code wrapping and reaching the end of a large document.
It runs in `native/update.sh`. `native/markdown-webkit.*` render the actual desktop
Markdown component for the same sample. See `PACKAGE-5.md` for the audited CSS
values, images, measurements, remaining differences and unapplied integration.


## Diff style controls (package 6)

`theme.diff`, `desktopDiffTheme`, `NativeDiffHeader`, and `DesktopDiff` match the
desktop plain unified diff while preserving native virtual rows. See
[PACKAGE-6.md](PACKAGE-6.md) for the API, refreshed unapplied integration patch,
dark/light WebKit pairs, bounded-paint checks, performance evidence and limits.


## Package 7: link controls and scene-local backdrops

```tsx
import './native/backdrop.ts' // HostProps augmentation
<anchored deferred backdrop={{blur:16,saturation:1.25,radius:16}}>
  <div style={{backgroundColor:'rgba(30,32,37,.78)'}}>…</div>
</anchored>
<markdown theme={{...theme,markdown:{...theme.markdown,
  link:{color:'#368ded',underline:'hover',weight:500}}}} source={source}/>
```

`theme.markdown.link` has optional `color`, `underline: 'none'|'always'|'hover'`,
and `weight`. Omission preserves native defaults. Both ordinary shaped prose and
mixed inline-code flow retain link byte ranges, selection, and per-link clicks.
Hover uses native mouse events and repaints on range transitions, with no timer.

`backdrop` works on ordinary elements and on the **anchored element itself**.
The latter filters inside the deferred layer and opts out of its otherwise opaque
fallback backing. Putting a filter on a child of an unmodified anchored surface
would sample that opaque backing. `{blur:0}` explicitly makes that backing
transparent without filtering. Use one filter per surface; keep fill/children above
it. `blur` is logical pixels (clamped 0–64), `saturation` is a ratio (0–3, default
1), `radius` clips the filtered corners, and `fadeTop` is the fraction of height
for a linear transparent-to-full filter fade (0–1, used by the live caption).

The macOS Metal backend gets an ordered scene primitive, not an AppKit subview.
It ends the current render pass, samples the already-rendered target into a cropped
horizontal Gaussian pass, then draws the vertical pass with saturation, content-mask
clipping, rounded bounds and inherited opacity before the surface's fill/children.
Scene replay retains the primitive for cached views. A draw-order barrier prevents
primitive batching from moving preceding text after the filter.

Two 49-tap separable passes approximate a Gaussian with a 3-sigma halo. One scratch
texture per renderer grows to the largest visible cropped region and is reused
across filters and frames on the ordered command queue. It is released when the
scene has no filters; compiled pipeline state stays cached for the window's lifetime.
Offscreen, unmounted and zero-opacity regions add no filter primitive. No continuous
repaint or polling is introduced; a changed frame still recomputes its visible
filters even when the change was elsewhere in the window.

Linux and Windows draw it too since WP9 (below). It samples GPUI's scene, not other
windows or the wallpaper under NSVisualEffectView. It does not add subtree blur,
scale transforms, arbitrary CSS filter strings or general mask-image support.
The Gaussian/underline rasterization is not pixel-identical to WebKit.

`native/patches/backdrop_gpui.py` reconstructs the GPUI changes from the pinned
submodule HEAD during `update.sh`, alongside the existing GPUIX hooks. No upstream
submodule commit or npm dependency is required. See `PACKAGE-7.md` for results,
`BACKDROP-AUDIT.md` for exact desktop values and exceptions, and the **unapplied**
`app-integration-7.patch` for the app changes. Run `bun native/check-integration-7.ts`
to typecheck that patch without editing app files.


### Backdrop alpha (ADE-58)

The vertical pass of the backdrop filter used to keep the destination alpha (`source_alpha_blend_factor(Zero)`,
`destination_alpha_blend_factor(One)`). Over a translucent window that left the scene's silhouettes in the alpha channel:
glyph pixels are opaque, the coat around them is not, so the desktop showed through crisp text-shaped holes under every
frosted surface (the "…" menu, the composer, pills) however well the colour was blurred. On Apple GPUs the pass now blends
in the fragment shader, `mix(dst, blurred, coverage)` on all four channels with the target read through `[[color(0)]]`, so
the alpha is blurred with the colour. A Metal function constant (`programmable_blend`, index 0) selects it per pipeline; other
GPUs (Intel Macs cannot read the target in a shader) keep the fixed-function blend and the old look.
`KYBERN_BACKDROP_PROGRAMMABLE=0` forces the old pass. `src/platform/verify.blur.ts` measures it: the "…" menu's window-alpha
detail over dense text is 0.33 with the old pass and 0.005 with the new one (`perf/translucency-ade58-2026-10-23.md`).

### Off macOS: wgpu and Direct3D 11 (WP9)

The filter draws on Linux (`gpui_wgpu`, `backdrop.wgsl`, `wgpu_backdrop.rs`) and Windows (`gpui_windows`, `backdrop.hlsl`,
`directx_backdrop.rs`) with the Metal algorithm: the same kernels and crop (`gpui::backdrop_kernel`, which Metal now uses
too; `packages/native/src/backdrop_kernel.rs` is the same file, compiled standalone by `update.sh`, which checks they match),
3-sigma halo, half-resolution path above 8 device px (`KYBERN_BACKDROP_HALFRES=0` keeps full size), saturation, own radius,
rounded clip chain with edge fades, fade-top and opacity. Per filter:

1. copy the target region behind it (clip plus halo, inside the target) into a pooled scratch: a swap chain image cannot be
   bound as a texture, so this replaces Metal's direct sample of the target, and it is cropped to the filter. The crop is
   copied exactly, odd sizes included: rounding it to the half-resolution grid either reached past the target or left the
   last column or row uncopied, where step 3 reads the destination (a full-width frost on an odd-width window under any
   coverage below 1: opacity, an edge fade, a rounded corner at the edge);
2. horizontal pass from the copy into a second scratch (`ceil(crop / scale)`: an odd crop's last half block reads its one
   copied column or row, both passes clamping their reads to the copied size). Crops under 2 px use full resolution;
3. vertical pass into the target over the clip, writing `mix(destination, blurred, coverage)` on all four channels with the
   destination read from the copy, blending off. That is the result of Metal's programmable blending (ADE-58), so a
   translucent Windows window (Mica) gets a blurred alpha instead of text-shaped holes, without framebuffer fetch.

The copy and the blur scratch are pooled separately, each following N1: 64 px buckets, released after 30 frames without a
planned filter, re-created smaller when the last 120 frames needed under half their area. The blur scratch is sized by the
largest `scratch_size`, so at half resolution it holds a quarter of the copy's texels. wgpu writes every filter's two
parameter blocks into one reused buffer, uploads them into one uniform buffer per frame (dynamic offsets) and rebinds only
when that or the clip chain buffer changed; Direct3D maps one constant buffer per pass. Shaders: wgpu compiles WGSL at runtime (naga-validated in
`gpui_wgpu`'s tests); `gpui_windows/build.rs` compiles `backdrop.hlsl` with fxc (vs/ps 4.1) like `shaders.hlsl`.

Fallbacks, each leaving the fill above the filter as before: wgpu on WebGL (no storage buffers) or a surface without
`COPY_SRC`, and `KYBERN_BACKDROP_FILTER=0` on both backends (the A/B control the `backdrop` check uses).
`kybernCapabilities().backdrop` tells the app this build's renderer can blur (`BACKDROP_BLUR` in `src/native/local-build.ts`;
false with `KYBERN_BACKDROP_FILTER=0`), and the renderer's `backdropFilterSupported()` whether the open window does
(`Window::backdrop_filter_supported` in the zed fork: false for the wgpu fallbacks above). The app reads it once the window is
up (`readWindowBackdrop` in `src/platform/material.ts`); without either, frosted surfaces keep their dense fills.

Checks: `cargo test -p gpui_wgpu --lib backdrop` in the zed fork validates the WGSL and renders two scenes through the
filter on whatever adapter the machine has (Metal on a Mac, lavapipe in a Linux container): stripes under a rounded filter,
and an odd 201 x 121 target under a half-coverage filter whose last column and row must keep the background (it fails
without the exact crop). `native/backdrop-check.ts` (`run-platform-checks --checks backdrop`) asserts blur, tint, radius and
chain clip, saturation, sharp later content, the odd 721 x 481 window (`BACKDROP_LAB_ODD=1`), the fallback and
`backdropFilterSupported()` on every OS, and measures frames and CPU per frame over a busy scene (`perf.json`).

### Window materials on Windows (WP9)

`setWindowBackground` and `windowBackground` also take `mica`, `mica-alt` and `acrylic` on Windows. On Windows 11 22H2+
(build 22621, `DWMWA_SYSTEMBACKDROP_TYPE`) GPUI sets the DWM system backdrop and extends the frame over the client area, so
the composition swap chain's transparent clear shows it; `blurred` is Acrylic there too, and each mode resets the other
mechanism (DWMSBT_NONE, accent 0, zero margins). Older Windows ignores the system backdrops and keeps the accent blur for
`blurred`. DWM tints both by `DWMWA_USE_IMMERSIVE_DARK_MODE`, which GPUI set from the system theme and reset on every
`WM_SETTINGCHANGE`; the renderer's `setWindowAppearance('dark' | 'light' | null)` (gpui `set_window_appearance`, now
implemented on Windows) sets it from Kybern's theme for every window and keeps it, while `window_appearance` still reports
the system's for "System". `src/shell/appearance.ts` calls it with the theme variant. The app picks the materials in
`src/platform/desktopMaterial.ts` / `src/shell/windowMaterial.ts` and Settings › Appearance (Backdrop: Mica or Acrylic; the
system sets their strength). They are opt-in (`KYBERN_DESKTOP_MATERIAL=dwm`, `DWM_BY_DEFAULT`) until seen on a real
Windows 11 desktop; without them Windows 11 is opaque like Windows 10. Linux has no compositor material standard: the window stays opaque and
the theme paints the Solid frame colour under the glass coats, so frosted surfaces still blur the app's own content.
`native/window-material-lab.tsx` (`MATERIAL_LAB_BACKGROUND=mica|acrylic|…`, `KYBERN_DWM_EXTEND_FRAME=1`) is the look on a real
Windows desktop; CI captures read GPUI's scene and cannot show what DWM composes behind it.

## Round 3: scoped leaf paint

`paint_replay.rs`, `paint_hooks.py`, and `paint_gpui.py` add a conservative
paint-only path to the pinned GPUI fork. `update.sh` reconstructs the GPUI edits
alongside `backdrop_gpui.py`; no upstream submodule commit is needed.

A fixed-size passive leaf with no children, text, event handlers, hover/active
styles, motion, backdrop, border or shadow can update either a div's solid
`backgroundColor` or an SVG's `opacity` without invalidating ancestors. The
renderer tags its original scene primitives with the element ID and inherited
opacity. A style-only batch validates every tag before changing quad fills or
monochrome-sprite tint, then requests presentation without element rendering,
Taffy layout, prepaint, hitbox rebuilding or focus changes. Scene replay keeps
clipping, layer order, atlas tiles, cached ranges and backdrop ordering intact.

Unknown paint, missing/unpainted/clipped tags, geometry changes, custom props,
structural updates and mixed batches use normal invalidation. Other platforms
and the test host keep ordinary revision behavior unless the production host
has enabled scene replay. The scene tags live only as long as the rendered
scene. `KYBERN_SCOPED_PAINT=0` is the full-render A/B reference.

This is **not a dirty-rectangle Metal compositor**: GPUI still submits the whole
scene. It removes ancestor render/layout work for the supported leaves; it does
not promise less GPU time or remove the application's React commits.

`native/paint-check.ts` uses **window-ID OS captures** for live pixel assertions:
GPUIX's automation screenshot method forces a full refresh and would hide stale
cached paint. The check covers repeated fills, SVG opacity, a genuinely changed
parent, movement, hit testing, focus/editing and removal/remount, in dark/light.
The existing native suite covers selection, overlays, scrolling, virtual rows,
layout observers, decorated inputs, native springs, Markdown and syntax.
See `ROUND3.md` for measurement results and the exact remaining work.


## Round 3 resume: ancestor pointers, clipboard, experimental frames

Implicit filled boxes now use an ancestor-preserving hitbox behavior. A painted
child continues to occlude unrelated overlapping controls but no longer makes its
parent lose hover or click routing. Explicit pointer barriers and deferred overlays
remain hard barriers. `pointer-check.ts` exercises dwell-before-click and overlap.

The native renderer additionally exposes `readClipboardImage(): {data: Uint8Array,
mediaType: string, name: string} | null` and `readClipboardPaths(): string[]` through
GPUI's clipboard. They do not change clipboard contents. The automation
`simulateFileDrop` accepts `{x,y,paths}` and dispatches the same native drop events
as the platform to existing `onFileDrop` handlers.

The frame APIs and `paintAnimation` prop are experimental; see `ROUND3.md` before
integrating their app changes. `subscribeAnimationFrames(callback)` returns a token;
the callback receives `(error,{timeMs})`. Acknowledge with
`acknowledgeAnimationFrame(token)` to allow the next delivery; unsubscribe with
`unsubscribeAnimationFrames(token)`. Deliveries are bounded, and consumers must
ignore already-queued stale callbacks after unsubscribe. `src/native/frame.ts`
shares one subscription and handles generations. A pending native delivery expires
after one second so a lost acknowledgement cannot silence it indefinitely; a
failed callback enqueue retries on the next frame. The shared JS clock also
replaces a subscription after one second without deliveries while visible and
in use. A generation guard lets a new subscription or acknowledgement restart
a lost native callback chain without duplicating a delayed chain. GPUI preserves
next-frame callbacks when a re-entrant window update fails, and logs that deferral.
`frame_clock_test.rs` checks enqueue failure, lost acknowledgement, lost chains
and late callbacks; `frame-check.ts` drops an acknowledgement in a real window.
Hidden-window behavior is a
required regression check, not something inferred from callback counts.

`paintAnimation: {kind:'matrix'|'breathe',cycleMs:number,delayMs?:number}` is allowed
only on passive fixed-geometry div fills or SVG tints. Native paint samples the
same easing without per-frame React updates, while still replaying/submitting the
whole scene. Reduced motion omits the prop. Embedded-host display wake and
inactive full-rate motion are enabled by default; `KYBERN_VSYNC_WAKE=0` and
`KYBERN_FULL_RATE=0` select the historical paths for comparisons.
`KYBERN_REGION_PAINT=1` opts into retained damage rendering; the target is allocated
only during eligible passive loops and released when they park. Path/filter
scenes use full rendering; it remains opt-in. Passive glyph tint
(`KYBERN_NATIVE_TEXT_PAINT`) is on by default since motion/fps; settled loops
then run in Core Animation ("Compositor loops"), which removed the GPU cost
that kept both opt-in. `frame-check.ts` captures actual window pixels plus bounded
submission cadence, callback backpressure, unsubscribe, minimize and unmount.

The original experiment is retained at `gpuix/native` `f9e5e2a7`. Round seven
integrates the frame source and bounded wake selectively, rather than merging
that checkpoint wholesale. See the round-seven report for before/after cadence,
idle, memory and four-mode checks. Callback timestamps are display-source ticks,
not physical presentation fences.

### PR scroll layout and input cadence

PR viewports opt in with `cacheLayout: true`. `scroll_layout.rs` and
`scroll_taffy.rs` reuse root-relative layout geometry for an unchanged row;
prepaint and paint still rebuild clipping, hitboxes, selection and search.
The cache checks retained identity/revision, available size, scale, styles,
topology and measured sizes. It retains no frame-arena IDs or elements, and is
bounded to 128 roots / 16,384 nodes (8,192 per root), expiring rows after two
frames outside the visible window. `KYBERN_SCROLL_LAYOUT=0` is the full-layout
control.

`scroll_input.rs` accumulates precise pixel wheel events for painted
`cacheLayout` viewports (PR, transcript and sidebar) in one pending native slot and applies a merged delta on the next
platform frame. GPUI List forwards its paint-bounds callback explicitly so
this gate uses the actual List viewport, including cached frames. Same-direction deltas sum; reversal takes the latest direction.
Phase markers preserve scroll-axis resets, and clicks/keys/target changes flush
pending input in order. The physical macOS input path and automation share
`Window::dispatch_event`; physical native-list scrolling has no JS/RPC hop.
`KYBERN_SCROLL_COALESCE=0` disables accumulation. `=all` is an experimental
app-wide precise-wheel mode; the default uses painted `cacheLayout` targets.

The native addon uses GPUI `test-support`, whose `App::flush_effects` normally
draws dirty windows after every update. The flick follow-up now skips that
synchronous test draw by default and relies on normal platform-frame scheduling
for every input/state update. It preserves event delivery, including pointer
moves and keys. `KYBERN_INPUT_FRAME_DRAW=0` restores the eager control, while
`1` explicitly selects the default. See `../perf/flick/` for paired measurements
and the broader interaction/visual validation.

With `KYBERN_FRAME_COSTS=1`, `[scroll-layout-frame]` reports geometry reuse and
`[scroll-input-paint]` reports native dispatch-to-CPU-paint latency and merged
packet counts. These are not physical presentation fences. `perf/scroll8.ts`
adds 120 Hz pipelined RPC production, acknowledgement backlog and drain metrics;
`native/scroll8-check.ts` checks pixel/selection/search/link parity, and
`native/scroll8-input-check.ts` checks burst distance and navigation ordering.

PR viewport islands additionally opt into GPUI's `independent_scroll_cache`.
Upstream GPUI forces nested cached views to refresh when any cached ancestor
re-renders; scrolling one list otherwise lays out the surrounding PR pane.
This opt-in distinguishes that internal refresh from a genuine window refresh.
The retained root revision invalidates the boundary on any content/style
change. During active native motion, these PR boundaries build inline so ancestor
opacity and transforms remain current. Other cached views retain their original behavior.
`KYBERN_SCROLL_ISLANDS=0` disables these boundaries for the baseline control.

Automation wheel requests accept optional `phase` values (`started`, `moved`,
`ended`, `cancelled`) through the additive native `simulateScrollPhase` API.
GPUI has no separate momentum flag. Zero-distance phase markers preserve the
existing list offset rather than resetting its captured-frame accumulator.

Scroll reproduction (capture-free timings, fair UI queue, no overlapping Rust builds):

```sh
bun perf/scroll8-all.ts
SCROLL8_REAL=1 SCROLL8_PR_NUMBER=109 SCROLL8_FILE_INDEX=15 bun perf/scroll8-all.ts
bun perf/scroll8-followup.ts
bun perf/scroll8-report.ts
```

The first command records four-mode full-app controls and the actual desktop
frontend in Playwright WebKit. The real-PR proxy forwards only read methods to
`~/.kybern`; preference writes stay in its disposable fixture. The follow-up
runs a quick app-wide scheduling control, PR actions, idle and separate
four-mode captures. Timed input is 120 Hz pipelined pixel deltas with native
contact/end phases and synthetic decay, rather than OS trackpad events.

## Rotate and pop paint animations (Phase 2, kit9)

Two more `paintAnimation` kinds ride the scoped leaf paint path above
(`animate_tagged` → `repaint_tagged_inner`): the scene re-evaluates the tagged
leaf at every presented frame with no element render, layout or prepaint.

- `{ kind: 'rotate', cycleMs }` on a passive fixed-size SVG: `animate-spin`.
  Each frame sets the sprite's `transformation` to a rotation about the sprite
  centre. It runs until the prop is removed; `src/kit/Spin.tsx` removes it under
  Reduce Motion and while the window is hidden, so a still glyph presents no
  frames. Used by the PR checks and the sidebar's running state.
- `{ kind: 'pop', cycleMs, delayMs? }` on a passive text or SVG leaf: `t-pop`
  (250 ms `--ease-bounce`, scale 0.85 → 1, opacity 0.4 → 1, `both` fill). Each
  frame sets every sprite's alpha and a scale about the centre of the union of
  the leaf's glyph sprites; 100 ms after the curve ends the leaf stops asking
  for frames. `src/kit/Pop.tsx` skips it under Reduce Motion.

Both are paint-only: geometry, layout, hit testing and clipping are untouched.
Only monochrome and subpixel sprites honour `transformation`, so a pop on a box
with a background or border only fades; there is still no subtree scale.

Files: `rotate_gpui.py` and `pop_gpui.py` (GPUI fork, run from
`backdrop_gpui.py` after `paint_gpui.py`/`paint_text_gpui.py`), and
`rotate_hooks.py` and `pop_hooks.py` (native crate, run from `apply_hooks.py`).
`pop_*` anchors on lines `rotate_*` writes, so keep the order. A lane that adds
another kind next to them must extend the same `mode != …` chain. Merge order
otherwise fails loudly at the anchor, never silently.

Fixed in render9 (`stream_fade_gpui.py`): a pop whose frames stopped part-way (window hidden during the
250 ms) lands on scale 1 / full opacity at the next presented frame (`finish_pops`), and a finished pop no
longer counts as a native paint animation, so region layers (`KYBERN_REGION_LAYERS=1`) can park. Checks:
`scripts/kit9-spinner-check.ts` and `scripts/kit9-pop.ts`.

## Rich editor and per-run text size (phase 2, editor9)

`<rich-editor>` (`patches/rich_editor.rs`, model `patches/rich_doc.rs`) is the one
formatted editor for notes, quick capture and task descriptions; the JS side and its
API are in `src/editor/README.md`. `editor9_hooks.py` only registers it: the module
lines in `lib.rs`, the factory in `custom_elements/mod.rs`, `"rich-editor"` in the two
focus-handle `matches!`, and
two napi methods before `zoom_window`: `richEditorCommand(id, command)` and
`setRichEditorImage(src, bytes)`.

- Keybindings are bound when the first editor renders, not at startup: bound globally
  they made `paint-check.ts` see ancestor island renders on paint-only updates.
- The model is std-only: `rustc --test patches/rich_doc.rs` runs its tests (Enter,
  Backspace, input rules, history grouping, composition as one step, paste).
- Layout is per block, cached by block revision and width; painting skips blocks
  outside the content mask. `stats` counts reshaped blocks (one per keystroke).
- The element implements `EntityInputHandler` over a flat UTF-16 view (blocks joined
  by `\n`), so marked text, candidate windows and commits go through the platform path.
- Images decode once into a shared, bounded cache (48 entries) and large ones are
  area-averaged to 1280px: GPUI samples without mipmaps, so a 2,560px screenshot
  shrunk to the column shimmers.

`text_scale_gpui.py` (chained from `backdrop_gpui.py`) adds per-run font size inside
one shaped line, which GPUI otherwise lacks (a line has one size). A `Font` whose
features carry `kszl = N` is shaped and painted at N/1000 of the line's size: the mac
text system records a scale per FontId (stripping the pseudo feature before CoreText),
`layout_line` applies it per run, shaped runs map back to the scaled FontId by point
size, and `paint_line` rasterizes glyphs at the scaled size. The editor uses it for
inline code (0.833em) and task references (13/15), like `.note-prose code` and
`.task-ref`. Ordinary fonts are unchanged (scale 1). DirectWrite and cosmic-text do the
same since WP8 (PORTING-PLATFORMS.md, "WP8 text: status").

## Paint-only subtree transforms (Phase 2, render9)

`paintTransform` on a host div scales, rotates, translates, fades and blurs everything the div paints, about a
transform origin, without moving its layout slot. Frames run on native display callbacks and re-map retained paint:
no React render, Taffy layout, prepaint, island wake or ancestor invalidation.

```tsx
<div paintTransform={{
  initial: { scale: 0.85, opacity: 0.4 },          // or false (start at `animate`)
  animate: { scale: 1, opacity: 1 },               // scale, rotate (deg, clockwise), translateX/Y, opacity, blur (px)
  transition: { duration: 0.25, delay: 0, ease: [0.34, 1.36, 0.64, 1] },  // seconds; y may leave [0, 1]
  keyframes: [{ offset: 0.6, rotate: 4, scale: 1.06 }],                   // optional CSS-style stops
  origin: [0.5, 0.5],                              // fractions of the div's box
  press: { scale: 0.97, down: { duration: 0.08 }, up: { duration: 0.15 } },  // native :active
  generation: 1,
}} onMotionComplete={e => …} />
```

- Changing `animate`, `keyframes` or `generation` retargets from the value on screen; a new `generation` with an
  `initial` value restarts from it (the skeleton band's next pass). `onMotionComplete` fires with `motionGeneration`.
- `press` squashes on a primary mouse down over the div (a hitbox under its children, so occluding overlays block it)
  and restores on any mouse up, natively: no React state. `src/motion/press.ts` (`pressTransform`) is the desktop's
  press feedback; `kit/Pop.tsx` `popTransform` the whole-box `t-pop`; `motion/success.ts` `t-success`;
  `motion/DropHighlight.tsx` the composer's drop ring; `kit/Skeleton.tsx` the shimmer band.
- How: `Scene::insert_primitive` maps each primitive painted inside a scope (and keeps its untransformed base); a
  retained update (`Window::transform_subtrees`) recomputes changed scopes from the bases and replays the scene.
  Quads, shadows, images, surfaces, backdrops and underlines scale about the origin (radii, borders, blur and
  thickness scale too); glyph and SVG sprites take the full affine in their `transformation`; paths transform their
  vertices. Opacity multiplies every primitive's alpha. Masks pushed inside the scope (rounded overflow) map
  through the transform; the mask at the scope's entry stays in window coordinates; a primitive keeps only the
  edges of its own clip that are narrower than those masks (ancestor clips are never mapped, so an `overflow:
  hidden` viewport stays put while a stage inside it pans: ADE-71's cut-off tall image). A border-only quad inside
  a scope stays one quad instead of GPUI's four strips, whose masks were cut by the ancestor clip before the
  transform. Hitboxes and AccessKit bounds follow the paint. Check: `bun native/pan-clip-check.ts`.
- `blur` paints a backdrop pass over what the subtree drew (bounds + 8 px), marked so frames set its radius; a
  settled blur renders once to drop the pass. Like CSS `filter: blur()` it is applied before the transform.
- Limits: rotation is exact for sprites and paths; quads/images/masks keep their axes (centres rotate), and
  hitboxes/masks use the rotated envelope, so rotate only glyph/icon subtrees. Layout observers and automation
  `getBounds` report the layout box. Text selection inside a transformed subtree uses untransformed geometry.
  An identity scope without `press` is not tagged, so its first change renders once; a nested
  `paintTranslate` inside an animating `paintTransform` moves by untransformed offsets.
- Files: `subtree_transform.rs` (native: prop parsing, evaluator, element, press, ticks), `subtree_transform_hooks.py`,
  `subtree_transform_window.rs` + `subtree_transform_a11y.rs` (GPUI), `subtree_transform_gpui.py`, `render9_js.py`
  (bridge: `paintTransform` and `onFileDragChange` props and types).
- Check: `bun native/subtree-transform-check.ts` (four modes; retained frames equal an ordinary render pixel for
  pixel, hit testing follows the paint, native press, frames without renders). Strips against the desktop's
  motion CSS in WebKit: `bun scripts/render9-strips.ts` then `bun scripts/render9-strip-metrics.ts <dir>`.

### Nested scopes in a replayed island (onboarding-retained, 2026-10-28)

A cached island that replays inside an active `paintTransform` scope (a scene's entrance, a `Presence` held at identity
with `retain`) used to keep only the active scopes: `Scene::insert_primitive` mapped each replayed primitive's base through
the active context, so every scope nested inside the island was dropped for that frame. An element held at opacity 0
(before its entrance, a `Presence` after its exit) presented at full opacity and a rise jumped to its rest position; the
onboarding showed both scenes at once on step changes. `Window::prepare_transform_replay` now composes, per recorded
context, the active scopes with the recorded scopes nested inside the range (their values and masks), and replayed hitboxes
follow the same context. `KYBERN_NESTED_SCOPE_REPLAY=0` is the A/B control. Check: `bun native/nested-replay-check.ts`
(and `KYBERN_NESTED_SCOPE_REPLAY=0 bun native/nested-replay-check.ts --control`, which must see the flash).

A retained `paintTranslate` inside a `paintTransform` scope (`translate_subtrees`, `subtree_geometry.rs`) moved the
presented primitive but not its transform base, so the next retained transform frame rebuilt it from the stale base and
the subtree jumped back to its old offset for one frame (the onboarding's project rows sliding during a scene entrance). The
base now takes the translation, before the transform, and the primitive is re-presented through its scope; hitboxes follow.
A scope inside the translation still transforms about its untranslated origin (exact for opacity and translation).

Not fixed, by design of the paint-only path: `paintTransform` opacity multiplies each primitive's alpha rather than
compositing the subtree as a group, so overlapping paint shows through while a scope is below 1 (the onboarding's window
material previews look translucent during a scene fade). Group opacity would need an offscreen layer per scope.

`KYBERN_PRESENT_DUMP=<dir>` (macOS, `gpui_apple` `metal_renderer.rs`) writes every presented drawable to
`<dir>/<frame>-<ms>.png` at half size, copied on the frame's own command buffer. It is what reached the screen, which the
automation screenshot (a fresh render) and `screencapture` without Screen Recording cannot show. Off and inert unless set;
the passive-region present path (`KYBERN_REGION_LAYERS=1`) is not dumped. `scripts/onboarding-present-dump.ts` uses it.

## Variable font weights (render9)

`font_weight.rs` / `font_weight_gpui.py` (gpui_macos): a face with a `wght` axis (SF Pro, the system UI font) renders
the requested CSS weight on that axis, as WebKit does, instead of font-kit's nearest named face (its CoreText trait
mapping put Medium at 530, so 500 matched Regular, and 600 drew Semibold = wght 590). The instance is registered
under its PostScript name + `#wght=` so CoreText runs map back to it; axis lookups and sized run fonts are cached
(uncached, `CTFontCopyVariationAxes` localized axis names on every shaped run). `KYBERN_FONT_WEIGHT_AXIS=0` restores
the old matching. Check: `bun native/font-weight-check.ts` (every ladder line within 1 px of WebKit's width).

## Paint-only streaming fade (render9)

The live answer's per-word fade (`.t-stream-w`: opacity 0 → 1 and `blur(1px)` → 0 over 350 ms smooth-out) no longer
re-renders the Markdown with faded run colours at every frame. `stream_fade_md.rs` hands the fading words' rects to
the scene (`Window::push_fade_words` / `end_fade_words`); glyph sprites inside them are tagged as they enter the scene
and `Scene::animate_fades` sets their alpha (and each young word's blur pass) in place before every present, in the
operation log and the sorted GPU buffers. Renders now happen only for text deltas and the caret blink.
`KYBERN_STREAM_FADE=render` selects the previous path for measurements. Colour emoji (polychrome sprites) do not fade.

## File drag hover (render9)

`onFileDragChange` (`file_drag.rs`): `hovered: true` when a Finder drag (GPUI external paths) moves over the element,
`false` when it moves off, drops, leaves the window or ends. The composer eases its drop ring with it
(`src/motion/DropHighlight.tsx`). `renderer.simulateFileDrag(x, y, paths, 'enter' | 'move' | 'end')` drives it in tests.

## Stroke draw-in (motion10)

`paintAnimation: { kind: 'stroke', cycleMs, delayMs?, dash, ease? }` on an `<svg source>` strokes its paths in like the
desktop's `t-check-draw` (`stroke-dasharray: dash`, `stroke-dashoffset: dash -> 0` over `cycleMs` after `delayMs`, on the
cubic-bezier `ease`, default `--ease-smooth-out`). `motion/success.ts` `strokeDraw` / `successStroke` build it (undefined under
Reduce Motion or with the stock addon); used by `motion/SuccessCheck.tsx` (`t-success`: sign-in success, answered approvals)
and `kit/Checkbox.tsx` (`.t-check-path`).

- Paint-only: the SVG's sprite is tagged as it enters the scene (`Window::with_stroke_draw`); before every present
  `Window::animate_strokes` points it, in the operation log, the sorted GPU buffer and any `paintTransform` base, at an atlas
  tile of the same SVG rasterized (resvg, like any SVG) with the dash attributes at the current offset. Offsets are quantized
  to 1/64 of a user unit; a draw rasterizes one small tile per presented frame and returns them to the atlas one frame after
  it ends, when the sprite goes back to the tile it was painted with. Geometry, order, clipping and hit testing never change.
- The clock is the view's automation clock (paused and stepped in strips, like `paintTransform`), and a glyph keeps its clock
  across re-renders (by element id); an unmounted glyph is forgotten after two seconds. Frames stop when it has ended.
- Files: `stroke_draw.rs` (native: prop parsing, wrapper, clock), `stroke_draw_hooks.py`, `stroke_draw_scene.rs` +
  `stroke_draw_window.rs` (GPUI), `stroke_draw_gpui.py` (runs after `stream_fade_gpui.py`).
- Check: `bun native/stroke-draw-check.ts` (four modes: no ink during the delay, partial mid-draw, monotonic, a re-render keeps
  progress, the end equals a plain render, display-rate frames without renders, none after the end). Strips against the
  desktop's motion CSS in WebKit: `bun native/ui-lease.ts motion10 bun scripts/motion10-strips.ts`.

## Bounded UI leases for update.sh

`native/ui-lease.ts <lane> <command…>` joins the fair UI queue only when it is about to run, waits for its line to be first
and the lock free, takes `/tmp/kybern-gpuix-round5-ui-lock`, runs the command with `KYBERN_PACING_PARENT_LEASE` and
`KYBERN_CLIP_LOCK_HELD` (so `clip-check`, `explorer-motion-check`, `scroll8-check` and `scroll8-input-check` reuse the lease
instead of queueing behind it), then leaves the queue and releases the lock. `update.sh` runs those four checks together
under one input lease as `update-input-<checkout>`. Geometry and content captures
have their own bounded leases from `check-update.sh`, so actual-window pixel
checks cannot race another lane's windows and each batch stays below five minutes.
Standalone Rust checks run before packaging, inside the marked build phase.
The lease owns a separate process group for its command. Interruption stops that
group before releasing the UI, and normal exit removes any orphaned descendants.
Inherited leases stay in the owning group. `bun native/ui-lease-check.ts` verifies
that interrupting a command also stops its grandchild.
Every lease, inherited or not, first waits while any other lane's
`/tmp/kybern-gpuix-quiet.*` file exists, so native checks never enter another
lane's final measurement window. The caller's own `/tmp/kybern-gpuix-quiet.<lane>`
is skipped: a lane that already marked its measurement window quiet takes the
lease under the same lane name instead of waiting on itself.

### Motion reconciliation during deferred rows

`motion_frame.rs` shares the global motion reconciliation across the root and
all deferred virtual rows in one GPUI draw. The key includes the retained-tree
revision, so a Node commit between root rendering and row prepaint still
reconciles the new motion description. Styles continue sampling the existing
clock, and completion notifications are emitted once. A per-window draw epoch
also covers cached scroll draws that do not rebuild the root view.
`KYBERN_MOTION_SYNC=0` is the same-binary control for the previous per-row global
scan; it does not disable motion.

Cached GPUI view ranges now carry their draw and rollback generation
(`cache_frame_gpui.py`). A List focus/autoscroll retry can discard prepaint
text and dispatch entries after a cached child wrote its new state. Such
state must render again; it cannot replay indices against the prior draw.
A rollback also invalidates caches recorded in the abandoned transaction.
Ordinary consecutive draws still reuse the unchanged island.

### Interior rounded-clip upload fast path

`gpu_clip_gpui.py`, `gpu_clip.rs` and `gpu_clip_bounds.rs` simplify only temporary
Metal instance-upload copies. An axis-aligned primitive whose clipped raster
rectangle is at least one device pixel inside every rounded ancestor's
conservative rectangular core needs no rounded-fragment evaluation. Edge and
corner primitives retain the original chain. Transformed monochrome sprites,
shadows, paths and backdrops retain their original masks. Scene operations,
replay/cache generations, hit testing and animation defaults are unchanged.
Scenes containing backdrops also keep the original upload masks for all
primitives: repeated streaming pairs did not show a consistent AGX improvement.
`KYBERN_GPU_CLIP_PRUNE=0` selects the same-binary control. The containment helper
has a standalone `rustc --edition=2021 --test` check.

`gpu_telemetry_gpui.py` adds opt-in `KYBERN_GPU_TRACE=1` scene/atlas counters.
`[gpu-scene]` records ordinary instanced draws, separate blur/composite draws,
primitive/vertex counts, candidate/pruned glyphs, atlas upload/eviction events,
rounded clip nodes and glyph chain steps/depth, fixed-record instance bytes and
clipped bounding area. The area is a workload
proxy, not a hardware overdraw counter. Existing `[paint-gpu]` traces report
command-buffer elapsed time; per-PID AGX GPU work is measured separately.
Diagnostics request no frames and remain off by default. Methodology, paired
measurements and limitations are in `perf/gpu/`.

## Display-paced dirty-window drawing (flick)

`flick_frame_gpui.py` selects the existing normal platform-frame path by
default. The addon enables GPUI's `test-support` feature for automation; its
eager dirty-window loop inside `flush_effects` can build/layout the same window
repeatedly before a display callback gets to submit a scene. Fast precise
wheel bursts exposed intervals with 8–16 native draws and no React commit.
Native wheel accumulation and the ordinary dirty/needs-present/next-callback
scheduling remain unchanged. `KYBERN_INPUT_FRAME_DRAW=0` restores the eager
same-addon control; `1` explicitly selects the new default. This switch adds
no idle clock, no overscan and no history cache. Measurements and correctness
evidence are in `../perf/flick/`.

`flick_bounds_hooks.py` preserves synchronous geometry inspection: explicit
`getAutomationTree` snapshots build a dirty window once through an unborrowed
window handle. Ordinary `getElementBounds` keeps reading the last painted bounds
and does not draw. This prevents the historical
`waitFor()` → `click()` missing-bounds failure when a host exists before its
first display paint. Ordinary updates and wheel packets still use native frames.

The same explicit snapshot barrier applies to screenshot capture before image
rasterization. The first end-to-end flick update caught an immediate-hover
image that still showed the prior frame; capture now paints the requested dirty
scene through an unborrowed window handle. Ordinary input and painted-bounds
reads remain display paced.

A window with a manually paused/set/advanced automation clock retains eager
deterministic test painting until `clock.resume()`. This preserves immediate
spring start/overshoot checks without enabling the test-support loop during
normal runtime input. The second complete updater detected that controlled-clock
compatibility issue; normal flick timing never pauses or advances the clock.

## Backdrop Gaussian kernels (perf-backdrop)

`backdrop_kernel.rs` and the appended `backdrop_kernel_gpui.py` hook retain the
existing two-pass filter, crop, halo, rounded masks, saturation, opacity and draw
ordering. Gaussian weights and normalization are computed once on CPU per
backdrop submission. Device sigma >8 retains all 49 original sample positions.
For sigma <=8, the original linear samples are reconstructed as texel weights,
then neighboring texels are paired into hardware-linear samples (<=25 taps;
Retina streaming word blur uses <=7). Neither material pixels nor scene damage
are cached; moving underlying content still filters every submitted frame.

`KYBERN_BACKDROP_KERNEL=0` selects the original fragment loop in the same addon.
No frame source, motion timing, theme value or accessibility gate changes. The
helper has standalone Rust tests across 804 sigmas and both clamp edges. Native
legacy/candidate high-contrast comparisons include shifted source, fractional
bounds and rounded clipping in dark/light across four window/accessibility modes;
the observed maximum difference is 2/255 per channel, not pixel identity.

Window/rail/sidebar materials already use AppKit's behind-window compositor
material. Streaming's nine scene backdrops are per-word blur animations, rather
than nine persistent window materials. Research, surface decisions, methodology
and acceptance results live in `perf/backdrop/`.

The reconstructed filter has a separate Metal fragment entry point. The resource
pair selects the legacy or weighted entry point; only that pair is instantiated.
`KYBERN_BACKDROP_KERNEL=0` retains the original fragment and skips the kernel
upload. This prevents the control and optimized path from sharing a shader with
both loops. Nonfinite/invalid kernels select the legacy entry point on CPU.

## Translucency round 2: scratch pool, half-resolution blur, edge fades (N1–N3)

Spec: `docs/specs/gpuix-translucency-r2.md` §4.1, §7. macOS/Metal only.

**N1 scratch pool** (`backdrop_renderer.rs`). One blur scratch texture per
renderer, sized in 64 px buckets and grown as the union of requests. Each
presented frame calls `BackdropResources::begin_frame`: a frame without a
backdrop marks the scratch `MTLPurgeableState::Volatile` (it leaves the process
footprint at once and is reused if a backdrop returns; a purged one is
re-created), and 30 presented frames without a backdrop release it. When the
largest request over the last 120 frames is under half its area (and it has
lived 120 frames) it is re-created at the bucketed size of that window. The old
behaviour dropped it on the first backdrop-free frame and never shrank it.

**N2 half resolution** (`backdrop_kernel.rs` `BackdropKernel::half`,
`backdrop_kernel_gpui.py`). Device sigma > 8 with the weighted kernel enabled:
the horizontal pass writes a half-size scratch (each texel a linear sample at the
centre of a 2×2 target block, tap offsets doubled), the vertical pass reads it
bilinearly in half-size units and writes full resolution with the same rounded
clip, fade, saturation and opacity. Kernels are dense Gaussians at sigma/2 out
to 3 sigma, cut at 48 texels (≤ 2.4 sigma cut, sigma/2 ≤ 20): the vertical pass
folds pairs into ≤ 49 linear reads; the horizontal pass uses unfolded integer
taps while they fit (sigma/2 ≤ 8) because a linear read between two half texels
of the full-size target is not their mix, and folds above that. Wider blurs keep
the 49 sparse positions on the half grid. `KYBERN_BACKDROP_HALFRES=0` restores
full resolution; `KYBERN_BACKDROP_KERNEL=0` (legacy control) never halves;
sigma ≤ 8 (stream-word fades) is unchanged. Deviation from the spec: the gate
compares with an exact CPU Gaussian, not with the full-resolution path, whose 49
taps sit 4 device px apart at blur 16 and alias on text (4.1/255 mean, 13 max
from the exact blur; half resolution 0.5 mean, 3 max).

**N3 `edgeFade`** (`edge_fade.rs`, `rounded_clip.rs`, `rounded_clip_gpui.py`,
`backdrop.metal`). `edgeFade={{top, right, bottom, left, insetTop, insetBottom}}`
(logical px) on any element multiplies the alpha of everything the element and
its subtree paint by r², r = clamp(distance to the edge / ramp, 0, 1), minimum
over the faded edges; an inset moves that edge inward and cuts beyond it. It is a
link in the rounded clip chain (`RoundedClip::kind` 1, ramps in `radii`, 40-byte
node): `rounded_clip_alpha` and the backdrop walk multiply the ramp, so nested
fades multiply; no pass, target or per-primitive change. Its content-mask
rectangle is unbounded, so it never clips and survives retained translation
(`subtree_geometry.rs`, `subtree_transform_window.rs` re-intern it with its kind,
transforms scale the ramps). Hit testing, native-surface shapes and automation
shapes skip it. GPU clip pruning and compositor loops treat a fade as satisfied
only where it is exactly 1 (`edge_fade_interior`). The prop is forwarded by
`src/native/local-build.ts` (adds `edgeFade` to `UNIVERSAL_PROPS` when the addon
reports it; the pinned JS patch predates it) and reported by the addon export
`kybernCapabilities() → {edgeFade: true}` (`EDGE_FADE`). Idea after ZUI's
`EdgeFade` (a Zed fork, Apache-2.0/GPL); re-implemented here, no code copied.

Checks (`NAPI_RS_NATIVE_LIBRARY_PATH` must name this build when the shell sets it):

```
bun perf/backdrop/halfres-check.ts   # N2 vs exact Gaussian; sigma ≤ 8 identical
bun perf/backdrop/scratch-check.ts   # N1 footprint within 2 MiB after 1 s, release, shrink
bun native/edge-fade-check.ts        # N3 alpha ramps, nesting, insets, scroll, retained translation
```

## Code-close allocation release (hands-on round 5)

`code_alloc.rs`, included by the appended `code_memory_hooks.py`, extends the
existing mapped allocator from 4 MiB to **64 KiB** buffers. Large code builds
many 1 MiB frame-arena chunks and intermediate tables; Darwin otherwise retains
their freed small-zone pages. `KYBERN_CODE_MEMORY=0` restores the 4 MiB boundary
in the same addon. `code_frame.rs` / `code_frame_gpui.py` also return excess
dispatch-node Vec capacity in both frame buffers after an eightfold workload
collapse; index-based node identities stay intact. `KYBERN_CODE_FRAME_MEMORY=0`
isolates the allocator-only path. `KYBERN_LARGE_ALLOCATIONS_VM=0` still selects
System for all allocations. These changes release allocations only; text layout,
syntax, selection, atlas keys and frame scheduling are unchanged.

The appended `code_memory_gpui.py` supplies read-only shaped-layout counts and
the real per-App arena capacity. `getMemoryStats().appElementArenaBytes` is the
App's arena; the retained `elementArenaBytes` compatibility gauge reads the
fallback thread-local arena. `syntaxCache` reports the bounded neutral cache;
registered syntax documents keep their separate `getSyntaxDocumentStats()` API.
Counters add neither timers nor frame requests. Run `python3 native/code-alloc-test.py`
after reconstruction for the default, old-boundary and disabled allocator tests.

Ownership proof, Zed comparison and caveats: `../perf/memory/HR5-RESEARCH.md`.

The same frame policy releases spare `LineLayoutCache` map/used-key capacity
at frame completion, while retaining all live keys and index order.
`textLayoutCapacities` reports those eight aggregate capacities separately from
`textLayouts`' four live counts. The cleared backup uses the active frame's
workload, preserving warm capacity while a large document remains open.

Code-close small allocations use a private Rust-only mimalloc 0.1.52 instance
(exact libmimalloc-sys 0.1.49; libc override/interpose features are disabled).
`KYBERN_CODE_SMALL_MEMORY=0` isolates the 64 KiB mmap + frame-capacity change
without the small allocator. `KYBERN_CODE_MEMORY=0` restores the full legacy
allocator/frame control in the same addon. When mapped buffers release at least
8 MiB, the existing AppKit pump coalesces one `mi_collect(true)` after destructors
run. It adds no frame or timer; `smallAllocator` reports its event count.

`code_collect_gpui.py` also notifies that private collector after the drawn
App's element arena has cleared. Destruction of a large reconciler batch or
release of a syntax/Markdown document requests one collection at that boundary;
a request survives an earlier AppKit pump. Large arena contraction returns its
unused chunks immediately. Nested deferred clears and event arenas do not
notify. `KYBERN_CODE_POST_FRAME_MEMORY=0` selects the earlier 8 MiB pump-only
policy in the same addon, including its original arena slack. Collection stays
event driven; there is no idle timer, frame request or glyph eviction.


## Hover reconciliation after layout (hands-on round 5)

`hover_reconcile_gpui.py` and `hover_reconcile.rs` deliver a dedicated native
hover reconciliation event after a completed layout. It reaches both replayed
cached listeners and the prior frame's retired listeners. Existing GPUI
`on_hover` and `:hover` state changes defer their callbacks/invalidation until
after drawing. Rebuilt hitboxes match by global element identity; retired
owners receive a leave. No mouse movement, drag or text-selection event is
synthesized. Retained translations/transforms use their existing presentation
callback to re-hit-test their updated geometry and update the cursor.

The pinned GPUI and the research Zed checkout already update `mouse_hit_test`
after prepaint, and uncached div painting already reconciles `on_hover`. Cached
paint bypasses that div code, while listeners that leave the rendered list
previously received no leave. The new event travels through the ordinary cached
mouse-listener ranges, including sparse rows, without a second registry, cache,
idle clock or timer. Notifications occur only on an actual hover transition.
`KYBERN_HOVER_RECONCILE=0` is a same-addon diagnostic control.

The sidebar uses one hover owner around each row and its revealed controls.
It no longer counts nested child enters/leaves, whose balance changes when a
control unmounts. Native ancestor-preserving hit testing supplies that ownership.

`hover_owner_gpui.py` and `hover_owner_hooks.py` keep that ownership across
cached-view and ordinary virtual-row paints. Those paths have different GPUI
global element IDs and can own different hover-state cells for the same retained
host. Every div/text host hitbox carries its renderer entity and retained host ID.
A retired listener sends no leave while its replacement hitbox is hovered;
relocated, occluded and removed owners still leave. This prevents an old path's
deferred leave from overwriting the replacement path's enter. It adds no timer
or frame source and leaves GPUI's focus, capture and ordinary hit-test identities
unchanged. Hover payloads also include the current pointer position, so pending
drag sources can distinguish leaving their bounds from a hover-state change.

`perf/hover/sidebar-check.ts` probes eight scratch projects with passive React
state snapshots before image capture, parked project/thread/coordinator hover,
stationary presses, small pointer jitter and intentional project drags. Its
test-only entry instruments the real sidebar without changing native input.
`sidebar-build.py` rebuilds under the whole-run native marker and isolates the
updater's data, port and captures. It uses native window-image capture when
WindowServer capture is unavailable; this does not establish OS presentation
latency. The focused passive snapshots guard against capture repairing hover.
`snapshot_strokes_gpui.py` and `snapshot_strokes_hooks.py` sample retained stroke
tiles after an explicit snapshot redraw, before image rasterization. This keeps
native captures faithful to paused animation clocks; normal presentation still
uses its existing callback. The unchanged stroke checker verifies delay, partial
ink, monotonic progress, the completed glyph and idle frames with this path.

The desktop's `styles/kybern.css:26–35` is the selection policy: chrome inherits
`userSelect: 'none'`; prose, Markdown and source bodies opt in to `'text'`.
`Label` also defaults to none for anchored UI. Native inputs and the rich editor
retain their own selection models. `selection_surface_hooks.py` also makes
custom code, Markdown and diff surfaces register their explicit selection
start region through `bounds_tracker`, just like host div/text nodes. Inherited
selectability alone could not override a select-none ancestor's start region.

`bun native/hover-check.ts` verifies stationary-pointer list retirement, the new
row's enter, layout reflow, retained translation/transform, prose selection, clearing a
selection on a control and parked frames in both themes. The list caches its
root like the sidebar; separate cached controls exercise paint replay. `perf/hover/` holds the
full-app audit, AppKit double-click bridge, native rebuild wrapper and benchmark
coordination. Results and limits belong in its report, not in timing claims
inferred from screenshots.

## Compositor loops (motion/fps, 2026-10-20)

Native paint loops (`paintAnimation` matrix / breathe / shimmer / rotate) used to present the whole window
at display rate: a scene replay, a full 2560×1640 GPU pass and a present 60 times a second (~3.5 ms GPU per
frame in the bench fixture). `compositor_loops_gpui.py` + `compositor_loops_renderer.rs` hand a loop to Core
Animation instead, the way WebKit runs CSS keyframes:

- **Eligible leaves:** every primitive of the tagged leaf is a solid borderless fill (`matrix`, `breathe`) or a
  monochrome sprite (`breathe`, `shimmer`, `rotate`: SVG glyphs and text glyphs; subpixel text stays in the
  scene); its bounds lie fully inside its content mask and every rounded clip (conservative containment as in
  `gpu_clip_bounds.rs`); any enclosing `paintTransform` scope is at rest (opacity 1, no blur; a moving one
  changes the bounds); nothing painted after it reaches its bounds (backdrop and shadow halos included; the
  leaf's own glyphs may touch). Pops, stroke draws and fades never qualify.
- **Hand-off:** `Window::present` promotes a leaf once all its parts have the same bounds on two consecutive
  presents; the scene repaints it transparent (`repaint_tagged_inner`), `animate_tagged` and
  `has_native_paint_animation` skip it, so with nothing else animating the window stops requesting frames.
  The renderer keeps one `CALayer` per part as a sublayer of the Metal layer: the fill colour with its
  corner radius, or the colour masked by the sprite's coverage tile copied once from the shared-storage glyph
  atlas (the shader's `color.a *= coverage`). A repeating `CAKeyframeAnimation` on `opacity` carries the
  native curve (CA's `easeInEaseOut` is CSS `ease-in-out`), the shimmer's 1 → 1.6 gain is sampled into 33
  keyframes relative to its clamped peak, and `transform.rotation.z` turns clockwise once per cycle. Its
  `beginTime` is the leaf's `started` instant plus delay, so the phase continues exactly.
- **Hand-back:** a new scene recomputes candidates per `paint_generation`. A leaf that moved, became
  clipped, got covered (a popup, tooltip or drag ghost), changed colour or unmounted leaves the set; any
  change to the set commits in one `CATransaction` with that Metal frame (`presentsWithTransaction` for that
  frame only), so a leaf is never drawn twice or not at all. Scrolling therefore paints loops in the scene
  and returns them to CA one frame after the list settles.
- **Off switches:** `KYBERN_COMPOSITOR_LOOPS=0`, a paused automation clock (strips), `KYBERN_PAINT_PHASE_MS`
  and `KYBERN_SCOPED_BUFFERS=0` keep scene painting. Screenshots (`captureScreenshot`) refresh the scene, so
  they paint the leaf themselves; OS window captures show the CA layers. `KYBERN_DEBUG_COMPOSITOR=1` logs
  candidates, rejections and layer counts.

Hidden and minimized windows: React already removes loop props there (`useLoopRunning`), and WindowServer
does not composite them. Check: `bun perf/motion-fps/lab-check.ts [--light] [--reduced]` (in `check-update.sh`;
also run with `KYBERN_COMPOSITOR_LOOPS=0` for the scene path). It asserts zero app frames once settled, one
layer per part, the curves and clockwise turn rates from live `presentationLayer` samples, upright glyph masks
(rendered in place with `renderInContext`), hand-back under a covering box, layout shifts and clean unmount.
Measurements: `../perf/motion-fps-2026-10-20.md`.

## One hover per link in inline flow (chatfix)

A paragraph with inline code is laid out by `markdown_inline.rs` as one text
fragment per word, and each fragment hit-tested and underlined only its own
slice of a hovered link, so a multi-word link underlined word by word.
`link_hover_hooks.py` (one `exec` line in `apply_hooks.py`) moves hover to the
flow: prepaint finds the hovered link once from the token geometry (binary
search to the pointer's line; a partly linked word is shaped once), every
fragment underlines its slice (`SelectableText::hover_range`, bridged into the
next fragment so there is no hairline gap), and one mouse-move listener per
flow refreshes only when the hovered link changes. Clicks stay per fragment.
Single-run paragraphs keep upstream behaviour. No timers; idle stays at 0
frames.

Check: `link-hover-check.ts` (first, middle and last word of a link wrapped onto
two lines, an inline-code link, the single-run control, both themes). This
session's environment may export `NAPI_RS_NATIVE_LIBRARY_PATH` for the
installed app's addon; `local-build.ts` then keeps it, so set the variable to
`native/build/gpuix-native.darwin-arm64.node` explicitly when checking a build.

## Motion native lane (gpuix-native, 2026-10-21)

### Motion latency trace

`KYBERN_MOTION_TRACE=1` (`motion_trace_gpui.py`, `motion_trace_hooks.py`, two lines in `subtree_transform.rs`) writes
`[mtrace] <ms> <event>` records on one clock: pointer and key input as GPUI dispatches it, each React batch, each render
(with the opacity of every layout-tween `motion` element), each retained `paintTransform` tick (scopes found and
missing), and each present (every retained scope's presented opacity). Off by default: one cached env check per event
and per present. `../perf/motion-native/latency.ts <label> model-picker reveal-exit` turns it into input → first paint,
input → first visible step, the mount frame's render and draw cost, renders per motion and fallback ticks.
`KYBERN_POPUP_MOTION=layout` (JS) is the control: every popup takes v0.10.1's drift-and-fade layout tween.

### Paint-only motion from its first frame (N0)

A `paintTransform` that mounts with an `initial` value is timed from the React batch that mounted it (`invalidate`
→ `sync`), so the frame in which its element first paints already shows a step; nothing waits for a retained tick.
Measured, the model picker (3,000-turn bench thread) shows its first visible step on its mount frame with no fallback
render; the layout tween paints its mount frame at opacity 0 and shows its first step one frame later. What remains is
the mount frame itself: the popup's island renders and lays out (9–39 ms of window draw there, layout 2–14 ms).

#### Motion latency

Measured with `../perf/motion-native/latency.ts` (Apple M1, hidden 1440×900@2x 60 Hz display, `KYBERN_FULL_RATE` default,
3,000-turn bench thread, dark, quiet window; addon `5ab4475d`; ms from the pointer event to the present):

| Action | Mechanism | First visible step p50 / p95 | Renders per action | CPU % / GPU ms per run |
| --- | --- | --- | --- | --- |
| Open the model picker (2 × 20) | layout tween (`KYBERN_POPUP_MOTION=layout`) | 63.3 / 77.0, 61.3 / 71.6 | 17 | 26.0 / 1,719, 25.4 / 1,701 |
| | paint-only (`paintTransform`) | 45.2 / 52.6, 44.9 / 61.0 | 3 | 22.1 / 1,863, 22.1 / 1,872 |
| Sidebar row hover-out (16) | before the N0 fixes (`3ece727b`) | 19.4 / 30.9 | 10 (144 fallback ticks) | 15.3 / 1,304 |
| | after | 22.5 / 29.7 | 2 (0) | 13.9 / 1,198 |

The paint-only popup is first visible on its mount frame (first paint = first visible step); the layout tween one
frame later. Its mount frame costs the same in both (30–32 ms of window draw, 10–11 ms layout): the next lever is that
frame, not the motion. GPU is ~8% higher on the paint path because the desktop's 2 px materialize blur runs for 250 ms.
Idle after every action: 0 presents. Pointer hover sweep down the sidebar (24 rows, two repeats, addon `75c1ec2a`):
hover fades on 31.3 / 32.4% CPU, 3,258 / 3,292 ms GPU; `KYBERN_HOVER_FADE=0` 31.7 / 28.4%, 3,413 / 3,218 ms; 40 presents
and 2.1 renders per row in both (the fades ride on frames the row's reveal already presents).

Two cases did lose frames, and are fixed:

- **An empty scope never counted as painted.** `has_subtree_transform` looked for primitives painted inside the scope,
  so a scope that draws nothing (the sidebar row's `FadeBack` around an idle row's empty status glyph) was "missing" on
  every tick and rendered the island every frame of its fade (12 renders per hover-out). The scene now records every
  scope it paints (`Scene::transform_scopes`, kept across full replays), and a scope that is still missing after one
  fallback render (hidden, never painted) waits for the next ordinary render instead of rendering every frame
  (`Track::fallback`).
- **A scope at rest was untagged,** so the first frame of its next change (an exit, a reveal hiding) was a render.
  `paintTransform.retain` keeps it tagged at identity; `presenceTransform` and `revealTransform` set it.

### Hover and press colour fades (N1)

`hoverTransition` (a host custom prop, inherited from the nearest ancestor that declares it; `false` opts out) gives
an element whose `hover` / `active` style changes a background or border colour a `ColorTransition`
(`hover_fade.rs`, `hover_fade_hooks.py`). GPUI (`hover_fade_gpui.py`, `hover_fade_window.rs`, `hover_fade_scene.rs`):

- `Interactivity::paint` keeps a `ColorFadeState` in the element's interactive state. A hover flip (or a press
  release) eases from the colour on screen to the new target; a press lands at once; a change of the element's own rest
  colours (state, keyboard selection, theme) is instant. The first step shows on the frame the flip renders (the fade
  starts one display frame back). Colours mix in premultiplied sRGB, like CSS.
- The element's background quad (kind 0) and, after its children, border quads (kind 1) carry a `ColorFadeTag` in the
  scene, kept through replay like the other tags. While a fade runs, a transparent end is painted at ε alpha so the quad
  exists. `Scene::animate_color_fades` re-tints them before every present, in the operation log, any `paintTransform`
  base and the sorted GPU buffer, and asks for frames until it has written the final colour once. No render, layout or
  prepaint runs between the flip and the end.
- The app declares `HOVER_FADE` (90 ms, `cubic-bezier(.4, 0, .2, 1)`) on its root (`HOVER_FADE_ROOT`);
  `KYBERN_HOVER_FADE=0` is the same-addon control. Colour fades are not motion, so Reduce Motion keeps them.
- Check: `bun native/hover-fade-check.ts` (dark, light, reduced, control: monotonic steps on the paused clock, a fill
  inside a retained `paintTransform`, opt-out, state change and press instant, release eased, one or two renders per
  hover, no frames after it).

### Rings that ease between values (N2)

`paintAnimation: { kind: 'stroke', dash, from, to, retarget }`: the draw runs between two dash offsets (`dash → 0` by
default). On a closed path of length `dash` with `dasharray: dash`, offset `dash · (1 − v)` shows the fraction `v` from
the path start. With `retarget`, a new `to` on the same glyph starts a run from the offset on screen (no delay), and a
glyph whose `to` is not zero rests on its `to` tile instead of the plain glyph. A 1 ms draw ends at once even on a
paused clock (Reduce Motion). `<circle>` paths are dashed too. JS: `ringValue` (`src/motion/success.ts`), used by the
rail usage rings, the composer's context ring and the account rings. Check: `bun native/ring-check.ts`.

### Embedded web documents fade and scale (N3)

`webview.m` op `present` animates the document clip layer's `opacity` and `transform` (scale about an origin) with a
`CABasicAnimation` from the presented value (or `fromOpacity`/`fromScale`) on the given cubic-bezier: WindowServer runs
it, the app presents nothing. `probe` reports the presented values (test seam). JS: `webHost.present`,
`webDocumentEnter` (150 ms entrance), used by the preview surface and visual replies. Check: `webview-check.ts`.

### True shimmer band and hue loop (N4)

`shimmer_band_gpui.py`, `shimmer_band_hooks.py`, `compositor_loops_renderer.rs`. `paintAnimation: { kind: 'shimmer',
cycleMs, highlight }` on a `Label`: every glyph sprite takes `mix(base, highlight, w)` for its own centre `u` across the
label, with `w = max(0, 1 − |u − (3t − 1)| / 0.4)` (the desktop's 400% gradient band at `background-position 100% → 0%`,
linear). In Core Animation each glyph layer animates `backgroundColor` through 49 keyframes sampled at its own `u`.
Without `highlight` the shimmer keeps its brightness pulse. `kind: 'hue'` rotates a leaf's hue once per cycle
(saturate 1.2), scene only. Check: `perf/motion-fps/lab-check.ts` asserts the band's left-to-right sweep from presented
layer colours.

### One-axis scale and shadow opacity (N5, N8)

`paintTransform` values gain `scaleX` / `scaleY` (multipliers of `scale` about the origin; axis-aligned primitives take
the map's column lengths, corners the smaller one, borders their own axis) and `shadowOpacity` (multiplies only
drop shadows). The switch's held thumb uses `scaleX: 1.1` (`kit/Switch.tsx`).

### Checklist press (N6)

`rich_editor.rs`: a pressed checklist box squashes to 0.92 about its centre (80 ms `--ease-out`) and returns on
release, skipped under Reduce Motion. Check: `src/editor/check.ts`.

### `<img onLoad>`

`image_load_gpui.py` (`ImageSource::kybern_loaded`), `image_load_hooks.py`: `load` fires once per source after the frame
its bitmap first landed in (`value`: `loaded` / `failed`). `Rise hold` waits on it, so a fresh agent image resolves from
its blur over the picture itself.

### Older cadence checks

`paint-region-check.ts` failed after minimize/resume only under Reduce Motion: its `settle()` returns at once there, and
the restored window could not be captured yet; the capture now retries. `scripts/kit9-spinner-check.ts` read GPUIX's
automation screenshot, which re-renders without the per-present rotation (every frozen phase looked alike); it now reads
the presented frame from WindowServer and expects a settled spinner in Core Animation (no app frames) unless
`KYBERN_COMPOSITOR_LOOPS=0`.

## Polish round 1 (lane polish1, 2026-10-21)

### Translated text keeps its off-clip glyphs (`translate_text_gpui.py`)

`paint_line` skips every glyph whose box misses the content mask. A retained translation (`paintTranslate`) moves only
what was painted, so the sidebar title marquee (`sidebar/motion.tsx` `MarqueeLabel`) slid the glyphs that fit the row's
clip out of view and never showed the tail: the title emptied mid-pan (the automation screenshot re-renders every frame
and hid it). Inside a translation scope (`Window::subtree_tags` non-empty) glyphs are now culled against the mask widened
sideways by 2048 px (`KYBERN_TRANSLATED_TEXT_REACH`); the mask still clips them in the shader. Text outside translation
scopes is unchanged. Check: `bun native/ui-lease.ts marquee bun scripts/marquee-check.ts` (real-time `screencapture -l` of
`scripts/marquee-lab.tsx`: ink across the row at the start, mid-pan and after the pan, the tail in its right half; fails
on `75c1ec2a`).

### `paintTransform` clip (`paintClip`)

`clipTop` / `clipRight` / `clipBottom` / `clipLeft` (px of the host's layout box, before the transform) and `clipRadius`
(px on screen, at any scale) round-clip a scope's subtree; they interpolate with the other values, so a clip can move,
resize and change its radius on retained frames. The clip is the host box inset, mapped through every scope up to and
including its own, and joins the context's mask chain (hit testing follows). No `clipRadius`, or a negative one, is no
clip: give it at both ends of a tween. `getMotionCapabilities().paintClip` advertises it. The shared image morph
(`src/motion/ImageMorph.tsx`) uses it to grow a cropped thumbnail into the preview with the thumbnail's radius.
Files: `subtree_transform.rs` (values, evaluator, host bounds), `subtree_transform_window.rs` (mask), `paint_clip_hooks.py`
(capability).

## Idle memory (ramfix, 2026-10-21)

`idle_memory.rs`, `idle_memory_hooks.py` (native crate) and `idle_memory_gpui.py` (GPUI), each run last from its
chain:

- **Per-tick autorelease pool.** Bun's main thread has no pool of its own, so whatever AppKit, CoreText or Foundation
  autoreleased outside `pump_events` (napi calls during React commits, `bun:ffi` reads from JS timers) stayed alive
  for the life of the process: the installed 0.10.1 app held 172k window-list arrays and 877 pool pages after
  13.75 h. `tick()` now pops the pool it opened on the previous tick and opens the next one, so JS and napi work
  between ticks runs inside it; AppKit's own pools nest above. `KYBERN_TICK_POOL=0` restores the old behaviour;
  `getMemoryStats().tickPool` counts cycles.
- **Window-state callback.** `setWindowStateCallback(cb)` / `acknowledgeWindowState()`: one queued delivery per change
  of occlusion (minimize, Spaces, covered), key status, position, screen, effective appearance, or the accessibility
  display options (an `NSWorkspace` observer, registered once). JS re-reads the state it needs
  (`src/native/windowState.ts`), so neither visibility nor the frame/appearance read polls any more.
- **`releaseIdleMemory(options?)`.** Drops what the next frame can rebuild: the window-sized path target (resident
  once any path was drawn; recreated when a scene next has paths), cached instance buffers (a grown pool restarts at
  2 MiB), backdrop scratch and the passive region target, and asks the private small-allocation heap to collect at
  the next frame boundary. `{ queue: true }` also replaces the command queue; it did not lower the driver's retained
  memory, so `src/platform/idleMemory.ts` only passes it with `KYBERN_IDLE_QUEUE=1`. Glyph and image atlases stay.

The JS side releases once the window has spent 60 s not key or hidden (`src/platform/idleMemory.ts`), adding a
full `Bun.gc(true)`; `Bun.shrink()` is opt-in (`KYBERN_IDLE_SHRINK=1`) because it discards compiled JS and slowed the
next opens. Measurements and the remaining driver-owned `IOAccelerator` memory: `../perf/ramfix/README.md`.


## Focus handles before an island or row builds (#218, 2026-11-04)

gpuix `issue-218` (one commit on c9647c3; applies cleanly on `issue-215`). A text input takes its focus handle on its
first render: the renderer's handle for its element (`sync_focus_handles`, run by `GpuixView::render` before it builds),
or, when there is none yet, a private one it keeps. In the frame a popup mounts, a virtual-list row (the sidebar's, in
the app) or a cached island can build before the root view renders, so the root's sync has not run for the new
elements. The model picker's search then rendered first with a private handle: autoFocus and `focusElement` focused
the renderer's handle, which nothing tracks (`context_stack()` was empty), and every key went to the window root. Seen
on Windows from the second open of the picker on. The island and virtual-list paths are shared with macOS; whether
macOS reaches the same frame order is not verified.

`sync_focus_handles_before_build` now runs at the start of `build_island_subtree` and `build_virtual_child`: when a
focusable element has no handle it creates the missing handles and runs their autoFocus, as `render` does; otherwise it
is only the lookups. Typing after a reopen: 1 of 4 opens before, 8 of 8 after; clicks in the field followed by a key
(`src/composer/picker-focus.verify.ts`, the real app on a scratch daemon): 10 of 30 lost before, 0 of 60 after, and
4 of 30 still lost with the JS change alone.

Separately, GPUI activates an `on_focus` listener on a deferred effect, so an element autoFocused in the frame its
subscription is created gets no `focus` event (every platform). `TextField` therefore no longer trusts a remembered
focus flag: a click on its box always asks for focus, which is a no-op for the focused input.
