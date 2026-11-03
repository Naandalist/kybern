"""Patch @gpuix/native's JS bridge and declarations atomically."""
import sys
from pathlib import Path
import hook_check
root = Path(sys.argv[1])
pending = {}
def edit(name, old, new):
    text = pending.get(name, (root / name).read_text())
    if text.count(old) != 1:
        hook_check.missing(f"{name}: expected 1 anchor(s), found {text.count(old)}; nothing written:\n{old}")
        pending[name] = text
        return
    pending[name] = text.replace(old, new)


def run_hook(file):
    hook_check.run(file, Path(__file__).parent, globals())

edit('dist/mutations.js', '    "motion",\n    "testId",', '    "motion",\n    // Kybern native capabilities.\n    "backdrop",\n    "paintAnimation",\n    "cache",\n    "windowDrag",\n    "interceptKeys",\n    "secure",\n    "softWrap",\n    "testId",')
edit('dist/host.d.ts', 'export interface HostProps {', 'export interface HostProps {\n    cache?: boolean | "fill";\n    /** Empty titlebar region; do not place on interactive controls. */\n    windowDrag?: boolean;')
edit('dist/mutations.js', '    "paintAnimation",', '    "paintAnimation",\n    "paintTranslate",')
edit('dist/mutations.js', '    let queue = [];', '    let queue = [];\n    const subtreeDeclarations = new Map();')
edit('dist/mutations.js', '        createElement(id, elementType) {', '        createElement(id, elementType) {\n            subtreeDeclarations.delete(id);')
edit('dist/mutations.js', '        setCustomProp(id, key, value) {', '''        setCustomProp(id, key, value) {
            // An unchanged React declaration must not overwrite an imperative
            // carry offset on an unrelated parent render.
            if (key === "paintTranslate") {
                const declaration = JSON.stringify(value);
                if (subtreeDeclarations.get(id) === declaration) return;
                subtreeDeclarations.set(id, declaration);
            }''')
edit('dist/mutations.js', '            onDestroyed(destroyed);', '            for (const id of destroyed) subtreeDeclarations.delete(id);\n            onDestroyed(destroyed);')
edit('dist/host.d.ts', 'export interface HostProps {', 'export interface HostProps {\n    /** Retained subtree translation; animate accepts translateX/translateY only. */\n    paintTranslate?: MotionProps;')
edit('index.d.ts', 'export declare class GpuixRenderer {', 'export declare class GpuixRenderer {\n  setSubtreeTranslation(elementId: number, translation: { translateX: number; translateY: number }): void')
edit('dist/host.d.ts', 'export interface InputProps extends HostProps {', 'export interface InputProps extends HostProps {\n    /** Key names delivered before native editing actions, with preventDefault(). */\n    interceptKeys?: string[];\n    /** Password input: masked paint/accessibility, no copy or cut. */\n    secure?: boolean;')
edit('index.d.ts', 'export declare class GpuixRenderer {', 'export declare class GpuixRenderer {\n  resolveKeyEvent(token: number, prevented: boolean): void\n  setCloseGuard(enabled: boolean): void\n  resolveCloseRequest(token: number, allow: boolean): void')
edit('dist/renderer-state.js', '        dispatch(event) {\n            const root = active;', """        dispatch(event) {
            if (event.eventType === "shouldClose") {
                Promise.resolve().then(() => state.shouldClose?.() ?? true)
                    .then(allow => renderer.resolveCloseRequest(event.requestId, allow === true))
                    .catch(error => { renderer.resolveCloseRequest(event.requestId, false); console.error(error); });
                return true;
            }
            if (event.eventType === "keyDown" && event.requestId != null) {
                // An intercepted key (`interceptKeys`) waits on native for the verdict.
                // It carries its own flags: the keystroke's ordinary keyDown follows.
                const flags = { prevented: false, stopped: false };
                const keyEvent = toKeyEvent(event, flags);
                const handler = active?.eventHandlers.get(event.elementId)?.get("keyDown");
                try { handler?.(keyEvent); active?.handlers.onEvent?.(keyEvent); }
                catch (error) { flags.prevented = true; throw error; }
                finally { try { renderer.resolveKeyEvent(event.requestId, flags.prevented); } catch (error) { console.error(error); } }
                return !!handler;
            }
            const root = active;""")
edit('dist/renderer-state.js', 'const STATE_KEY =', """/** Install a close guard. Rejections veto; repeated close attempts coalesce. */
export function setWindowCloseHandler(renderer, handler) {
    const state = createRendererState(renderer);
    state.shouldClose = handler;
    renderer.setCloseGuard(!!handler);
    return () => {
        if (state.shouldClose === handler) {
            state.shouldClose = undefined;
            renderer.setCloseGuard(false);
        }
    };
}
const STATE_KEY =""")
edit('dist/renderer-state.d.ts', 'import type { EventPayload }', 'export declare function setWindowCloseHandler(renderer: NativeRenderer, handler: () => boolean | Promise<boolean>): () => void;\nimport type { EventPayload }')

edit('dist/host.d.ts', '"setWindowKeyEvents" |', '"resolveKeyEvent" | "setCloseGuard" | "resolveCloseRequest" | "setWindowKeyEvents" |')
edit('dist/host.d.ts', '"setWindowKeyEvents" |', '"setSubtreeTranslation" | "setWindowKeyEvents" |')
edit('dist/host.d.ts', 'export interface CodeProps extends HostProps {', 'export interface CodeProps extends HostProps {\n    /** Wrap highlighted lines at the available width. */\n    softWrap?: boolean;')
edit('dist/host.d.ts', 'export interface MotionStyle {', 'export interface MotionStyle {\n    translateX?: number;\n    translateY?: number;')
edit('dist/host.d.ts', 'export interface MotionStyle {', 'export interface MotionStyle {\n    /** SVG rotation, in degrees about the centre. */\n    rotate?: number;')
edit('dist/host.d.ts', 'export interface MotionTransition {', 'export interface MotionTransition {\n    stiffness?: number;\n    damping?: number;\n    mass?: number;')
edit('index.d.ts', 'export declare class GpuixRenderer {', 'export declare class GpuixRenderer {\n  /** rotate refers to SVGs; arbitrary div-subtree rotation is not supported. */\n  getMotionCapabilities(): { rotate: true; physicalSprings: true }')
edit('dist/host.d.ts', '"setWindowKeyEvents" |', '"getMotionCapabilities" | "setWindowKeyEvents" |')
edit('dist/host.d.ts', 'export interface MotionTransition {', 'export interface MotionTransition {\n    type?: "tween" | "spring";\n    /** Damping control from 0 (no overshoot) to 1. */\n    bounce?: number;\n    /** Translation velocity in logical pixels per second. */\n    velocityX?: number;\n    velocityY?: number;')
edit('dist/automation/client.js', '        scrollWheel: (params) => {', """        fileDrop: (params) => {
            this.renderer.nativeSimulateFileDrop(params.x, params.y, params.paths);
            return { ok: true };
        },
        scrollWheel: (params) => {""")
edit('dist/automation/client.js', '        nativeSimulateScrollWheel(x, y, deltaX, deltaY, modifiers) {', """        nativeSimulateFileDrop(x, y, paths) {
            renderer.simulateFileDrop(x, y, paths);
            afterInput();
        },
        nativeSimulateScrollWheel(x, y, deltaX, deltaY, modifiers) {""")

edit('dist/automation/protocol.js', '    scrollWheel: {', '    fileDrop: {\n        params: pointSchema.extend({ paths: z.array(z.string()).min(1) }),\n        result: okSchema,\n    },\n    scrollWheel: {')
edit('dist/automation/protocol.d.ts', '    readonly scrollWheel: {', '    readonly fileDrop: {\n        readonly params: z.ZodObject<{ x: z.ZodNumber; y: z.ZodNumber; paths: z.ZodArray<z.ZodString> }, z.core.$strip>;\n        readonly result: z.ZodObject<{ ok: z.ZodLiteral<true> }, z.core.$strip>;\n    };\n    readonly scrollWheel: {')
run_hook("scroll_js.py")
run_hook("render9_js.py")
run_hook("motion_native_js.py")

for name, text in pending.items():
    if not hook_check.CHECK or not hook_check.problems:
        (root / name).write_text(text)
hook_check.finish(f"JS patch applied to {len(pending)} files")
