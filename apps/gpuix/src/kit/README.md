# `src/kit/` — shared GPUIX primitives

Primitives other lanes (settings, composer, transcript, dialogs, platform) build on. Each one was ported from the
retired Tauri app's `components/kit/*` (removed after 0.18.0; read it in git history before that release): geometry,
copy, states and motion came from that source and were checked against it in images. They are now the reference.

## Conventions

- One component per file, named export, documented props in this README (one section per component, below).
- Colours from `useTheme()` (`src/theme.ts`; cite the `kit.css`/component line for a new token). Text is `Label`.
- Every interactive part gets a `testId`. Pointer hover is instant unless the desktop eases it.
- Reduce Motion: every tween has a path (`REDUCED_MOTION` from `ui.tsx`): fades stay, movement goes.
- Popups are `PopupLayer`s (opaque, anchored; `composer/Menu.tsx`); tooltips and hover cards go through
  `kit/hoverController.ts` so only one is visible app-wide.
- Never a JS timer or React commit driving visible movement per frame: use `motion.div` tweens or paint animations.

## The control system (ADE-55, Direction C "Tinted state")

One set of controls for the whole app. Colour means state: the commit action and every selection share one translucent accent tint, never
a solid slab; neutrals are ink alphas, so every control composites on any surface including whole-window glass (the dark column serves
Solid dark and glass alike). Spec: `.kybern-artifacts/ade-55-design/spec.md`. A live board of every component is
`gallery/controlsBoard.tsx` (`KIT_SECTION=controls-board`, captured by `bun scripts/kit-controls-board.ts <out> [dark|light|both]`).

**Tokens (`controls.ts`).** `RADIUS` (xs 5, sm 8, md 9, lg 11, xl 13, field 9, check 5, kbd 5, pill 999, segTrack 11, segThumb 9),
`HEIGHT` (xs 20, sm 24, md 28, lg 32, xl 36), `SPACE` (the 4 pt grid), `CONTROL[size]` (per `sm|md|lg`: height, padX, iconPad, label
and glyph size, gap, radius) and `useControls()` / `controlsFor(theme)` (the colour tokens: `primary`, `secondary`, `ghost`,
`destructive`, `danger`, `disabled`, `focus`, `field`, `chip`, `segmented`, `check`, …). Nothing hard-codes 8/10/14 or 28/32 on a
control any more. Hover, pressed and keyboard focus (`focusVisible`, a 2 px accent outline) are native styles with no React state; only
the ghost label and icon buttons track the pointer, because a native hover cannot recolour a child.

**States.** rest, hover (one fill step up), pressed (a further step plus `SCALE.press`), `focusVisible` (keyboard only), selected/on
(tint, never colour alone), disabled (the disabled fill and `text3`, no hover, press or shadow: a disabled primary no longer looks
primary), busy (label kept, a 12 px spinner leads it, disabled).

**One-primary rule.** At most one `primary` per surface (card, dialog, bar, header, composer, settings section). On a confirm surface the
destructive action is the primary (`destructive`) and Cancel/Keep is `secondary`. A safe primary with a destructive alternative:
the alternative is `secondary danger` on the leading edge, Cancel + primary trailing. Lowest weight is `ghost`. The primary owns ⌘↵
(`kbd="⌘↵"`). Menus never contain buttons.

| Component | Props | Replaced |
| --- | --- | --- |
| `Button` | `variant` primary/secondary/ghost/destructive, `danger`, `size` sm/md/lg, `icon`, `iconAfter`, `chevron`, `kbd`, `busy`, `disabled`, `swap`, `trailing`, `block`, `onKeyboard`, `label` | `transcript/kit` Button, `settings/controls` Button, `usage/controls` Button, `KitButton`, `EnvButton`, `GuardButton`, `PaneButton`, tray `HeaderButton`, Notes `HeaderButton`, and the one-off pills |
| `IconButton` | `icon`, `label` (required), `size`, `active`, `secondary`, `glyph`, `color`, `disabled`, `tabIndex`; mouse handlers compose with its hover, so a `Tip` can wrap it directly | sidebar `IconButton`, title-bar `ChromeIcon`, `ChromeButton`, editor toolbar, terminal toolbar, `AddButton`, `Toggle` (deleted) |
| `SplitButton` | `variant` primary/secondary, `size`, `onClick`, `menu` (the `MenuItem`s), `menuAlign` | the PR Merge pair, Notes compose + chevron |
| `PickerChip` | `icon`/`mark`/`dot`/`lead`, `secondary`, `chevron`, `open`, `selected`, `neutral`, `quiet`, `muted`, `size` sm/md, `truncate`, `maxWidth` | `FooterChip`, `TrayChip`, Notes `Chip`, the Tasks scope chip, the model trigger, "Queue ⌄", the dock empty-state chips |
| `TokenChip` | `onRemove`, `lead` | the Computer-use app chips, `Combobox` chips |
| `Checkbox` | as before (`indeterminate`, `invalid`), `tone` accent/neutral; `onCheckedChange(checked, { shift })` | the Tasks `Check`, the criteria box, `WorktreeCheck`, the close-guard box, coordinator glyph checkboxes |
| `Radio`, `RadioRow` | `checked`, `title`, `description`, `onSelect` | the question-card and PR-review radios |
| `Segmented` | `size` sm/md, options with `icon` (label shown as a tooltip) and `count`, `fill`, `tone` accent/neutral | `settings/controls` Segmented, `usage/controls` Segments (non-tab uses), the Tasks view toggle |
| `TextField`, `SearchField`, `NumberField`, `TextArea`, `FieldShell` | `size` md/lg, `invalid`, `disabled`, `bare`, `leading`, `trailing`, `width` | `settings/controls` TextField/NumberField, the palette, settings, PR list and Notes searches, `AnswerField`, the PR review and environment textareas, `EnvInput` |
| `SelectTrigger` | the display name as children, `mark`, `size`, `open`, `placeholder` | the `settings/controls` Select trigger, `kit/Select`'s trigger, the usage period button |
| `Kbd` | `keys` | `nav/overlay` Kbd (18 high, one chip per key, a 1 px ink edge) |

`Slider` (Settings only; the model-picker effort slider is unchanged) draws the 4 px track, the accent fill and the 16 px thumb; `Switch`
keeps its 32 × 20 geometry with no off-track border, the new thumb shadow and a native focus ring. The nav rail is unchanged by design.
Tasks and Notes keep their quiet look: `PickerChip quiet` (transparent at rest, a neutral ink fill and the chevron on hover) for the
property pickers and `neutral` for selected filters, and `Checkbox`/`Segmented` take `tone="neutral"` (ink fill, spec §3.3), so their
selection stays ink; accent stays on the commit action, focus and
selection elsewhere.

Known gaps: a disabled `TextField` shows the ink edge only (the spec's 45° hatch needs a repeating pattern the renderer lacks).

## Status, people and inspector pieces (ADE-85, the pull request sidebar)

- `tone.ts`: `Tone` (`success` | `info` | `warning` | `danger` | `neutral`) and `toneColor(theme, tone)`: green, blue, amber, red and a quiet ink. Verdicts,
  chips and check glyphs read the same mapping.
- `Avatar` (`login`, `url?`, `size` 16): GitHub's picture when there is one, else the first letter on a hue taken from the login. The picture is fetched once
  per url into the image cache (`transcript/imageFiles.ts`) and painted from that file; only `https` urls, and GitHub's CDN is asked for twice the slot.
  A failed fetch keeps the letter for the session. 1 px white edge at 10%.
- `StatusChip` (`tone`, `icon?`, `lead?`, `dot?`, children): the tone at ~11% behind a 14 px glyph (or a 7 px label dot, or a ready-made `lead` mark such as a `ProviderMark`) and the label in normal ink; 22 high, radius 6. Machine cards use it neutral for specs and harnesses.
- `Card` (`tone?`): radius 13, padding 14, 10 between parts. A tone tints it (7% fill, 17% edge); without one it is the neutral card (`cardEdge`).
- `InspectorGroup` / `InspectorRow` / `SectionHeader`: hairline-separated groups of a 76 px label column and its values, as `tasks/TaskDetail.tsx` does inline.
- `AgentAction` (children, `onClick`): the quiet "Fix with agent" / "Address feedback" action: accent tint 16% (24% hover), 22 high, radius 7, a sparkle.
  It sits where the problem is, so a card with three still has one primary.
- `SplitButton` gained `icon` (a leading 16 px glyph source) and `block` (full width: the label part grows and centres).

## Verifying (the gallery)

`src/kit/gallery/` renders a named *section* of the kit with GPUIX; states (`visual/kit/steps/*.ts`) say how to drive
it. (The Tauri side of these image pairs was removed with the Tauri app.)

Motion: GPUIX strips with
`clock.pause()` + `clock.set(at + ms)` + `screenshot` (`scripts/kit9-notelift.ts`); `startGallery` in `scripts/kit9-lib.ts`
opens a section window. Frame cadence: `KYBERN_FRAME_TIMES=1` (`app.frameTimes()`), window kept visible with
`opts.visible`. Native paint animations (spinner) run on the real clock: freeze with `KYBERN_PAINT_PHASE_MS`.

## Hover popups (`hoverController.ts`, `hover.tsx`, `HoverCard.tsx`)

`useHover({ delay, closeDelay?, skip?, hoverable? })` returns `{ ctl, state }`; `hoverTrigger(ctl, key, element, pressRef, focusOpens)`
puts the handlers on a trigger; `HoverCard` is the generic `PreviewCard` (props: `trigger`, `children`, `side`, `align`,
`sideOffset` (8), `delay` (600), `closeDelay` (300), `skip`, `width`, `style`, `rootStyle`, `testId`, `disabled`).
`Tip` (`nav/overlay.tsx`) is the tooltip. State lives in plain fields, never read from a render closure, so an enter
and a leave inside one frame still close (R4-1). `ctl.leave(key)` ignores a late leave from a trigger that is no longer the
target, so a shared card with several triggers (the rail usage rings) survives GPUI delivering the next enter first.

## Spin (`Spin.tsx`)

`<SpinIcon source size color spinning periodMs testId />`: a glyph turning once per `periodMs` (1000) on the native
`rotate` paint animation (`the fork commit from rotate_gpui.py`); still when hidden, minimized or under Reduce Motion.

## Menus (`nav/overlay.tsx`)

`Menu`, `ContextMenu`, `MenuItem`, `MenuLabel`, `MenuSeparator`, `MenuCheckboxItem`/`MenuRadioItem` as before, plus
`<MenuSub id label icon? width=192 testId? disabled? maxHeight?>…items…</MenuSub>`: a row with a trailing chevron that opens a
nested popup (Base UI `SubmenuTrigger`: 100 ms hover open, a safe triangle from the pointer to the popup so a diagonal move
does not close it, →, Enter and Space open it and light the first item, ← / Esc close one level). The geometry is `nav/menuGeometry.ts`
(unit tests in `menuGeometry.test.ts`). Pointer handling reads geometry on each move; no timers run while nothing hovers.

## Dialogs and sheets (`AlertDialog.tsx`, `Sheet.tsx`)

`AlertDialog` (props `open`/`defaultOpen`/`onOpenChange`, `width` 512, `testId`) with `AlertDialogHeader`, `AlertDialogTitle`,
`AlertDialogDescription`, `AlertDialogFooter` (`variant: 'default' | 'bare'`), `AlertDialogClose` (`variant` outline, `onClick`).
`Sheet` (`side` right/left/top/bottom, `variant`, `showCloseButton`, `dismissible`, `width` 448) with `SheetHeader`, `SheetTitle`,
`SheetDescription`, `SheetPanel`, `SheetFooter`, `SheetClose`. The alert card fades and scales from 0.98; the sheet fades and slides
32px; both take 200 ms in and out. `ModalLayer` runs them as paint-only `paintTransform`s on the layer's content (the anchored
surface itself carries no fill, so a scrim's colour belongs in the content `style`, where it fades at its true alpha; no layout or
render per frame). `nav/overlay.tsx`'s `Dialog` does the same through `PopupLayer appear="dialog" | "scrim"`: scrim fade 250 ms,
card from 0.96 with a 2 px blur, exit 150 ms (0.98). A pointer close (the X, a press outside, `useDialogClose()`) plays the exit and
then calls `onClose`; Escape and `instant` (shortcut-opened) close at once; a caller that unmounts the dialog itself closes at once.
They keep an Escape stack (the innermost
overlay closes first: a `Select` inside a `Sheet`; `useEscapeLayer(open)` returns `claim()` for Escape and `isTop()` for outside
presses, and `nav/overlay.tsx`'s `Dialog` defers to `kitOverlayOpen()`) and return focus to the opener. Neither traps Tab yet.
The window size is read only while one is mounted (`WindowSized`). `ModalLayer`, `useEscapeLayer`, `usePresence`, `WindowSized`
are exported for other dialogs; the footer buttons are the one `Button` (`AlertDialogClose`/`SheetClose` map the desktop's variant names with `controlVariant`).

## Form controls

- `Switch` (`checked`/`defaultChecked`/`onCheckedChange`, `disabled`, `aria-label`): 32×20 track, 12 px thumb travel, press stretch.
- `Checkbox` (same plus `indeterminate`, `invalid`): the check strokes itself in (`.t-check-path`), painted natively by
  `paintAnimation: { kind: 'stroke' }` (`motion/success.ts` `strokeDraw`; no render or JS per frame).
- `Segmented` (`options: {value,label,icon?,count?,unavailable?}[]`, `value`, `onChange`, `busy`, `size` md/sm, `segmentPadX`, `fill`): radiogroup with the sliding tinted thumb;
  arrow keys skip unavailable options.
- `TimePicker` (`value` "HH:MM", `onChange`, `width` 168): two scrolling columns.
- `Select` (`options: SelectEntry[]` = `{value,label,disabled?} | {group} | {separator:true}`, `value`/`defaultValue`/`onValueChange`,
  `placeholder`, `size` xs/sm/default/lg, `variant`, `surface` settings/composer, `side`, `align`, `sideOffset`, `width`, `label`):
  item-aligned popup like Base UI, type-ahead, check indicator. A popup the pointer opened scales in from 0.98 (150 ms, about the
  selected row when it is laid over the trigger) and leaves in 80 ms; opened or closed from the keyboard it is instant.
- `Combobox` (`items`, `value`/`onValueChange`, `multiple` (chips), `placeholder`, `showTrigger`, `showClear`, `emptyText`,
  `itemToLabel`, `filter`, `width`): filtering input with a popup list and the empty state.

All take `testId` and (except the popups) `style`.

## Feedback

`Badge` (`variant`, `size` sm/default/lg, `icon`, `onClick`), `Skeleton` (`width`, `height`, `radius`, `still`; the light band moves
on one native tween, attached to the window like `background-attachment: fixed`), `Spinner` (`size` 24, `color`, `spinning`),
`Empty` + `EmptyHeader`, `EmptyMedia` (`variant` default/icon, `icon`), `EmptyTitle`, `EmptyDescription`, `EmptyContent`
(the header children rise in together, 12 px, 500 ms; `play={false}` for none).
`ProblemState` (`title`, `body`, `primary`, `secondary`, `detail`): a page that couldn't load, in plain words, with the
action that fixes it and the technical text behind "Show details" and a copy button, never as the headline. The pull
request surfaces map an RPC error's `data.problem` to it (`pulls/PrStates.tsx`, copy in `pulls/problemCopy.ts`).

## Resize (`Resize.tsx`)

`const col = useResize({ initial, min, max, side: 'left' | 'right', storageKey?, axis? })` gives `{ width, dragging, setWidth,
onPointerDown, reset }`; `<ResizeHandle edge label dragging onPointerDown onKeyDown onReset valueNow testId />` is the 16 px hit
area with the 2 px line on hover/focus/drag (arrow keys 16 px, Shift 64 px, double-click resets). `<ResizeCaptureLayer />` is mounted
once in `App.tsx` and follows the pointer during a drag (GPUIX only delivers mouse moves to the element under the pointer).
Used by the Explorer tree (176-520) and the PR list (288-512); the thread sidebar, dock and split dividers keep their own handles
(`shell/ResizeHandle.tsx`, `dock/`, `split/`), all driven by `scripts/kit9-resize-check.ts`.

## Tabs (`Tabs.tsx`)

The one tab strip (ADE-55 §2.7, Direction C): the in-thread strip (`tabs/Strip.tsx`), the dock, the terminal pane, the PR
detail and the model picker. `<Tabs items active onSelect onClose? onDoubleClick? size="md"|"sm" focused markSingle underlineGap
dividerAfter overflow minWidth gap dropIndex trailing wrapTab registerTab stripRef onTabMouseDown/Move/Up animate testId />`.
Items: `key`, `label`, `icon` | `mark` | `iconNode`, `preview` (italic, drawn as a Markdown emphasis because GPUIX text has
no `fontStyle`), `pinned` (no ✕), `dirty` (6 px dot until hover), `detail`/`extra` (counts, diff stats), `disabled`,
`iconOnly`, `tooltip`, `testId`, `closeTestId`. Geometry: 28 (sm 24) high, padding 8 (sm 7), glyph 14, gap 6, label 13/500
(sm 12/500), width 96–200 (pinned up to 320), ✕ 16 with radius 5 on the active and hovered tab. The active tab of a
focused strip has no fill and a 2 × (width − 16) accent underline `underlineGap` below it (the strip's bottom edge); in
an unfocused split group the underline turns to ink and the label to secondary text (`markSingle` underlines a lone tab,
so every group of a split marks its active tab). Overflow scrolls sideways with a 16 px `edgeFade`
and a » menu; the active tab is kept in view (instantly: no per-frame JS). A tab that appears after mount fades in from
0.97 (150 ms, paint-only; the neighbours step aside because GPUIX tweens no `maxWidth`), a closed one shrinks from its
measured width to 0 (120 ms); Reduce Motion: opacity, 90 ms. Switching is instant.

## Attention pop (`Pop.tsx`)

`<Label paintAnimation={popAnimation()}>`: the desktop's `.t-pop` (scale 0.85 → 1, opacity 0.4 → 1, 250 ms `--ease-bounce`) on a
text or icon leaf, as a native paint animation (`the fork commit from pop_gpui.py`, the same per-frame path as the spinner: no React or
layout per frame, frames stop when the curve ends). It plays when the leaf mounts; give it a new `key` to play again.
`usePopOnChange(active)` is true once `active` turned on after first render (virtual rows do not re-pop on scroll-in).
Leaves only: a box with a background does not scale (no subtree scale in the renderer yet).

- `HoverCard`, `Tip` (`nav/overlay.tsx`), `Menu`, `ContextMenu` and `MenuSub`: popups stay mounted through their exit (`PopupLayer`
  `leaving`/`onLeft`): menus 150 ms to 0.99, tooltips and hover cards 80 ms. A close from the keyboard (Escape, Enter on an item)
  unmounts at once; a tooltip or card that opened `instant` leaves at once too; submenus leave with the root menu.
- `Switch` and `Segmented` (equal-width segments) move or fade as paint-only transforms; `Badge` pops a numeric
  label when it changes after mount (`useCountPop`).
- `dock/loading.tsx` `LoadingLines`, `useAfter`: a skeleton that only shows after 150 ms of waiting (never a flash for fast reads).

## Motion helpers outside `kit/`

- `motion/themeFade.tsx`: theme switch cross-fade. `snapshotBeforeThemeChange()` before flipping the theme (done by
  `setAppearance` and the system-appearance poll); `<ThemeFade />` is the last child of the window root.
- `sidebar/motion.tsx` `MarqueeLabel`: a truncated row title pans to show its tail on hover (350 ms delay, 28 px/s).
- `kit/Spin.tsx` `SpinIcon`: the spinning glyph.
