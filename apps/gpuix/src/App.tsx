import { SplitWorkspace, SplitDragLayer, trackThreadDrag, finishSplitDrag, useSplitDragging } from './split/Workspace.tsx'
import { startSplitView } from './split/store.ts'
import { useMeasureComposerOverlay } from './composer/height.ts'
import { CloseGuard } from './shell/CloseGuard.tsx'
import { CloseDirtyDialog } from './tabs/CloseDirty.tsx'
import { WebPageUi } from './native/WebPageUi.tsx'
import { WorkspaceDialogs } from './dialogs/Dialogs.tsx'
import { OnboardingLayer, useOnboardingGate } from './onboarding/Welcome.tsx'
import { HandoffController, useShellReady } from './onboarding/Handoff.tsx'
import { useFlow } from './onboarding/flow.ts'
import { EnvironmentDialogs } from './environments/Dialogs.tsx'
import { useEnvironment } from './shell/environment.ts'
// The window shell (`App.tsx`, `views/chrome.tsx`): a 46px title bar and a 52px
// rail painted on the frame surface, and one 12px-radius card holding the thread
// panel and the page. All geometry follows the desktop. The title bar, rail,
// palette and dialogs live in `nav/`; the thread panel in `Sidebar.tsx`.
import { EnvironmentOverlay } from './environment/Panel.tsx'
import { FILL_ISLAND, ISLAND } from './MatrixLoader.tsx'
import { motion } from '@gpuix/react'
import type { PaintTransformProps } from '@gpuix/native/host'
import { useEffect, useState, type ReactNode } from 'react'
import { NotesPage } from './notes/NotesPage.tsx'
import { SettingsScreen } from './settings/SettingsScreen.tsx'
import { UsagePage } from './usage/UsagePage.tsx'
import { PullRequestsPage } from './pulls/PullRequestsPage.tsx'
import { TasksDragLayer, trackTaskDrag, useHeldTask } from './tasks/drag.tsx'
import { TabDragLayer } from './dock/reorder.tsx'
import { finishTabsDrag, moveTabsDrag, TabsDragLayer, useTabsDragging } from './tabs/drag.tsx'
import { ProjectDragLayer } from './sidebar/reorder.tsx'
import { TasksPage } from './tasks/TasksPage.tsx'
import { TasksSidebar } from './tasks/TasksSidebar.tsx'
import { startPreview } from './shell/preview.ts'
import { useTasksFeed } from './tasks/store.ts'
import { NativeSubagentComposer } from './subagents/Composer.tsx'
import { MoveBar } from './fork/MoveBar.tsx'
import { SubagentBar, SubagentPageMotion } from './subagents/Page.tsx'
import { Composer } from './Composer.tsx'
import { PreviewIntegration } from './preview/Preview.tsx'
import { ComputerLiveView } from './live/ComputerLiveView.tsx'
import { Dock, DockResizeCapture } from './dock/Dock.tsx'
import { Draft } from './Draft.tsx'
import { Icon } from './icons.tsx'
import { N } from './nav/glyphs.ts'
import { HandoffDialog } from './nav/Handoff.tsx'
import { startNavHistory } from './nav/history.ts'
import { startSpaces } from './spaces/current.ts'
import { startSpaceKeys } from './spaces/keys.ts'
import { startLive } from './nav/live.ts'
import { NavRail } from './nav/NavRail.tsx'
import { Palette } from './nav/Palette.tsx'
import { SessionsDialog } from './nav/Sessions.tsx'
import { getNav, pointerNav, useNav, type Page } from './nav/store.ts'
import { markUserInput } from './platform/userInput.ts'
import { EASE, HOVER_FADE_ROOT, Rise } from './motion/kit.tsx'
import { drawerSlide } from './shell/motion.ts'
import { EXIT, Presence } from './motion/Presence.tsx'
import { PATCHED_NATIVE } from './native/local-build.ts'
import { TitleBar, usePanelOpen } from './nav/TitleBar.tsx'
import { Sidebar } from './Sidebar.tsx'
import { startAgentToasts } from './shell/agentToasts.ts'
import { startRequestProgress } from './requests/progress.ts'
import { ConnectionBanner, useWelcome, Welcome, type WelcomeKind } from './shell/Connection.tsx'
import { ErrorBoundary } from './shell/ErrorBoundary.tsx'
import { useSidebarDragging, useSidebarWidth } from './shell/layout.ts'
import { SidebarDragLayer, SidebarResizeHandle } from './shell/ResizeHandle.tsx'
import { ResizeCaptureLayer } from './kit/Resize.tsx'
import { ThemeFade } from './motion/themeFade.tsx'
import { Toaster } from './shell/toasts.tsx'
import { startStarPrompt } from './shell/starPromptApp.ts'
import { AppUpdateSurface } from './platform/UpdateSurface.tsx'
import { NetworkIntroSurface } from './machines/NetworkIntro.tsx'
import { TranscriptReleased } from './platform/TranscriptReleased.tsx'
import { installWindowSurface, useTranscriptReleased } from './platform/surface.ts'
import { useApp, usePaneAccess, usePaneCompact } from './state.ts'
import { useLaunch, useLaunchMotion } from './draft/Morph.tsx'
import { COLUMN, GUTTER, Transcript } from './Transcript.tsx'
import { ThreadRequests, useBlockingRequest } from './transcript/Requests.tsx'
import { Label, REDUCED_MOTION, useMaterial, useTheme } from './ui.tsx'
import { RAIL, TITLEBAR } from './nav/TitleBar.tsx'
import { ResizeEdges } from './nav/WindowControls.tsx'
import { SINGLE_FILL } from './platform/translucency.ts'

const INSET = 8
const CARD_RADIUS = 12

/**
 * `KYBERN_TRANSLUCENCY_SINGLE_FILL=1` (perf A/B, `platform/translucency.ts`): in Sidebar only and Solid the frame colour is
 * painted only where the frame shows: the title bar, the rail, the right and bottom gutters, and the four corner squares the
 * card's rounded corners leave open. Nothing is painted under the workspace card, so each pixel gets one fill. The strips
 * partition the window outside the card, so a translucent frame is never blended twice.
 */
function FrameFill({ color }: { color: string }) {
  const at = (style: object) => <div style={{ position: 'absolute', pointerEvents: 'none', backgroundColor: color, ...style }} />
  return <>
    {at({ left: 0, right: 0, top: 0, height: TITLEBAR })}
    {at({ left: 0, top: TITLEBAR, bottom: 0, width: RAIL })}
    {at({ right: 0, top: TITLEBAR, bottom: 0, width: INSET })}
    {at({ left: RAIL, right: INSET, bottom: 0, height: INSET })}
    {at({ left: RAIL, top: TITLEBAR, width: CARD_RADIUS, height: CARD_RADIUS })}
    {at({ right: INSET, top: TITLEBAR, width: CARD_RADIUS, height: CARD_RADIUS })}
    {at({ left: RAIL, bottom: INSET, width: CARD_RADIUS, height: CARD_RADIUS })}
    {at({ right: INSET, bottom: INSET, width: CARD_RADIUS, height: CARD_RADIUS })}
  </>
}

// The thread panel slides as a drawer (`drawerSlide`); Reduce Motion, and toggles from the keyboard, change without movement.

/** Any press makes later navigation pointer-driven again, and counts as the user being here (`platform/userInput.ts`). */
function onAppMouseDown() {
  markUserInput()
  pointerNav()
}

export function App() {
  const env = useEnvironment()
  return <Workspace key={`${env.id}:${env.revision}`} />
}

function Workspace() {
  const splitDragging = useSplitDragging()
  const taskDragging = !!useHeldTask()
  const tabsDragging = useTabsDragging()
  useTasksFeed()
  const t = useTheme()
  const fills = useMaterial()
  const singleFill = SINGLE_FILL && t.scope !== 'window'
  const page = useNav((s) => s.page)
  const settingsOpen = useNav((s) => s.settingsOpen)
  const instant = useNav((s) => s.panelInstant || s.keyboardNav)
  const panelOpen = usePanelOpen() && !settingsOpen
  const sidebarWidth = useSidebarWidth()
  const resizing = useSidebarDragging()
  const welcome = useWelcome()
  // While the first-run flow is open the shell is not mounted at all: GPUIX lays out every element every frame, and a sidebar
  // filling with imported chats would cost each frame of the flow. It mounts under the opaque welcome just before it dissolves.
  const onboarding = useOnboardingGate()
  const releasing = useFlow((s) => s.releasing)
  const shell = !onboarding || releasing
  useShellReady()
  // The slide plays only when the panel opens or closes; a drag or a window resize moves it at once.
  const [seenOpen, setSeenOpen] = useState(panelOpen)
  const [sliding, setSliding] = useState(false)
  if (seenOpen !== panelOpen) {
    setSeenOpen(panelOpen)
    setSliding(true)
  }
  // Keep the panel's rows mounted only while it shows or animates: hidden
  // elements still cost layout on every frame in GPUI.
  const [panelMounted, setPanelMounted] = useState(panelOpen)
  if (panelOpen && !panelMounted) setPanelMounted(true)
  const still = REDUCED_MOTION || instant
  if (still && !panelOpen && panelMounted) setPanelMounted(false)
  const edge = t.cardEdge

  useEffect(() => {
    startSplitView()
    startNavHistory()
    startSpaces()
    startPreview()
    startStarPrompt()
    return startSpaceKeys()
  }, [])
  const connectionStatus = useApp(s => s.status)
  useEffect(() => {
    startLive()
    startAgentToasts()
    startRequestProgress()
    installWindowSurface()
  }, [connectionStatus])

  return (
    <div
      testId="app-ready"
      {...HOVER_FADE_ROOT}
      // kybern.css:26–35: chrome inherits select-none; document bodies opt in.
      // Any press makes later navigation pointer-driven again (`keyboardNav`).
      onMouseDown={onAppMouseDown}
      onMouseMove={taskDragging || splitDragging || tabsDragging ? (event) => { if (taskDragging) trackTaskDrag(event); if (splitDragging) trackThreadDrag(event); if (tabsDragging) moveTabsDrag(event) } : undefined}
      onMouseUp={splitDragging || tabsDragging ? (event) => { if (splitDragging) finishSplitDrag(event); if (tabsDragging) finishTabsDrag(event) } : undefined}
      style={{ userSelect: 'none', display: 'flex', flexDirection: 'column', width: '100%', height: '100%', backgroundColor: singleFill ? undefined : fills.frame, position: 'relative' }}
    >
      {singleFill && <FrameFill color={fills.frame} />}
      {shell && <>
      <TitleBar panelOpen={panelOpen} />
      <div style={{ display: 'flex', position: 'absolute', left: 0, right: 0, bottom: 0, top: TITLEBAR, paddingRight: INSET, paddingBottom: INSET }}>
        <NavRail />
        {/* One rounded clipping boundary owns every workspace surface, including
            split panes and the narrow-window dock overlay (kit.css:172-198). */}
        <div testId="workspace-card" style={{ display: 'flex', flexGrow: 1, minWidth: 0, borderRadius: CARD_RADIUS, overflow: 'hidden' }}>
          <motion.div
            initial={false}
            animate={{ width: panelOpen ? sidebarWidth : 0 }}
            transition={still || resizing || !sliding ? { duration: 0 } : drawerSlide(panelOpen)}
            onMotionComplete={() => {
              setPanelMounted(panelOpen)
              setSliding(false)
            }}
            style={{ display: 'flex', height: '100%', flexShrink: 0, overflow: 'hidden' }}
          >
            {/* Cached islands (native/README.md): the panel, the content card, the title
                bar and the rail each replay while another of them animates. */}
            <div
              {...ISLAND}
              style={{
                display: 'flex',
                width: sidebarWidth,
                height: '100%',
                flexShrink: 0,
                backgroundColor: fills.panel,
                borderTopLeftRadius: CARD_RADIUS,
                borderBottomLeftRadius: CARD_RADIUS,
                borderTopWidth: 0.6,
                borderLeftWidth: 0.6,
                borderBottomWidth: 0.6,
                borderColor: edge,
              }}
            >
              {panelMounted && (
                <ErrorBoundary label="the sidebar" compact>
                  {page === 'tasks' ? <TasksSidebar /> : <Sidebar />}
                </ErrorBoundary>
              )}
            </div>
          </motion.div>
          <div
            {...FILL_ISLAND}
            style={{
              display: 'flex',
              flexDirection: 'column',
              flexGrow: 1,
              minWidth: 0,
              overflow: 'hidden',
              backgroundColor: fills.content,
              borderWidth: 0.6,
              borderColor: edge,
              borderTopRightRadius: CARD_RADIUS,
              borderBottomRightRadius: CARD_RADIUS,
              borderTopLeftRadius: panelMounted ? 0 : CARD_RADIUS,
              borderBottomLeftRadius: panelMounted ? 0 : CARD_RADIUS,
            }}
          >
            <div style={{ display: 'flex', flexGrow: 1, minHeight: 0, minWidth: 0, position: 'relative' }}>
            <div style={{ display: 'flex', flexDirection: 'column', flexGrow: 1, minWidth: 0, minHeight: 0, position: 'relative' }}>
            <PageArea page={page} settingsOpen={settingsOpen} welcome={welcome} />
            <ConnectionBanner />
            </div>
            {/* Settings covers the workspace, dock included, as on the desktop. */}
            {!settingsOpen && <Dock sidebarOpen={panelOpen} />}
            {!settingsOpen && page === 'home' && <ComputerLiveView />}
            <PreviewIntegration />
            </div>
          </div>
          {panelOpen && <SidebarResizeHandle />}
        </div>
      </div>
      </>}
      <OnboardingLayer show={onboarding} />
      <HandoffController />
      <Palette />
      <HandoffDialog />
      <SessionsDialog />
      <EnvironmentOverlay />
      <EnvironmentDialogs />
      <WorkspaceDialogs />
      <CloseGuard />
      <CloseDirtyDialog />
      <WebPageUi />
      <Toaster />
      <AppUpdateSurface />
      <NetworkIntroSurface />
      <SidebarDragLayer />
      <ResizeCaptureLayer />
      <DockResizeCapture />
      <TasksDragLayer />
      <ProjectDragLayer />
      <TabDragLayer />
      <SplitDragLayer />
      <TabsDragLayer />
      {/* Linux client-side decorations: the window's resize edges, over everything (`nav/WindowControls.tsx`). */}
      <ResizeEdges />
      <ThemeFade />
    </div>
  )
}

/** Settings is a layer over the workspace (`App.tsx:325-333`): it enters 8px from the right (spring, 300 ms) and leaves 6px to the right in 120 ms. */
const SETTINGS_ENTER = (): PaintTransformProps | undefined => {
  if (!PATCHED_NATIVE) return undefined
  if (REDUCED_MOTION) return { initial: { opacity: 0 }, animate: { opacity: 1 }, transition: { duration: 0.15, ease: [...EASE.out] } }
  // `paintTransform` has no spring: a critically damped one (`bounce: 0`) settles like smooth-out, which is what this uses.
  return { initial: { opacity: 0, translateX: 8 }, animate: { opacity: 1, translateX: 0 }, transition: { duration: 0.3, ease: [...EASE.smoothOut] } }
}
const SETTINGS_EXIT = { to: { opacity: 0, translateX: 6 }, duration: 0.12 } as const

/**
 * The page on screen. A switch from the rail or a menu rises in like `t-section-enter` (6px, 250 ms smooth-out); the first page,
 * and switches from a shortcut, appear at once. Settings is a layer over it: opened or closed by the pointer it slides in and
 * out (the workspace page rises in as it leaves); by ⌘, or Escape it appears and goes at once.
 */
function PageArea({ page, settingsOpen, welcome }: { page: Page; settingsOpen: boolean; welcome: WelcomeKind | null }) {
  // The latest settings toggle: was it the keyboard, and did settings just close (the workspace page then enters)?
  const [toggle, setToggle] = useState({ open: settingsOpen, keyboard: false, closed: false })
  if (toggle.open !== settingsOpen) setToggle({ open: settingsOpen, keyboard: getNav().keyboardNav, closed: !settingsOpen })
  return (
    <>
      {!settingsOpen && (
        <PageEnter page={page} entry={toggle.closed && !toggle.keyboard}>
          <ErrorBoundary label="this page" key={page}>
            {welcome ? <Welcome kind={welcome} /> : <PageView page={page} />}
          </ErrorBoundary>
        </PageEnter>
      )}
      <Presence
        show={settingsOpen}
        enter={toggle.keyboard ? undefined : SETTINGS_ENTER()}
        exit={toggle.keyboard ? { ...SETTINGS_EXIT, duration: 0 } : SETTINGS_EXIT}
        style={settingsOpen ? { flexGrow: 1, minHeight: 0 } : { position: 'absolute', left: 0, right: 0, top: 0, bottom: 0 }}
      >
        <ErrorBoundary label="this page" key="settings">
          <SettingsScreen />
        </ErrorBoundary>
      </Presence>
    </>
  )
}

/**
 * A page switch from the rail or a menu rises in like `t-section-enter` (6px,
 * 250 ms smooth-out). The first page, and switches from a shortcut, appear at once.
 * `entry`: the page was just uncovered by closing settings with the pointer.
 */
function PageEnter({ page, entry = false, children }: { page: string; entry?: boolean; children: ReactNode }) {
  const [shown, setShown] = useState({ page, play: entry })
  if (shown.page !== page) setShown({ page, play: !getNav().keyboardNav })
  return (
    <Rise key={shown.page} kind="section" play={shown.play && shown.page === page} style={{ flexGrow: 1, minHeight: 0 }}>
      {children}
    </Rise>
  )
}

/** The rail destination's page. */
function PageView({ page }: { page: Page }) {
  switch (page) {
    case 'home':
      return <SplitWorkspace><HomePage /></SplitWorkspace>
    case 'notes': return <NotesPage />
    case 'tasks': return <TasksPage />
    case 'usage':
      return <UsagePage />
    case 'pulls':
      return <PullRequestsPage />
    default:
      return <ElsewherePage page={page} />
  }
}

function HomePage() {
  return <SubagentPageMotion><ConversationPage /></SubagentPageMotion>
}

function ConversationPage() {
  const selectedId = useApp((s) => s.selectedId)
  if (!selectedId) return <Draft />
  return <ThreadPage selectedId={selectedId} />
}

/** An open thread: the transcript with the composer floating at the bottom. Mounted from the new chat, it plays the send
 *  transition its launch describes (`draft/Morph.tsx`). */
function ThreadPage({ selectedId }: { selectedId: string }) {
  const gutter = usePaneCompact() ? 20 : GUTTER
  const measureComposerOverlay = useMeasureComposerOverlay(1)
  const thread = useApp((s) => s.open.thread)
  const blocking = useBlockingRequest()
  // A hidden window releases its transcript (`platform/surface.ts`).
  const released = useTranscriptReleased()
  const pane = usePaneAccess()
  // Until the transcript loads, the listed thread already gives the composer its thread (no draft composer flashes in).
  const listed = useApp((s) => (s.open.thread ? null : s.threads.find((th) => th.id === selectedId && !th.subagent) ?? null))
  const launch = useLaunch(pane.id, selectedId)
  const motion = useLaunchMotion(launch)
  return (
    <>
      {released ? <TranscriptReleased /> : (
        <ErrorBoundary label="the transcript">
          <Transcript />
        </ErrorBoundary>
      )}
      {motion.under}
      <div ref={measureComposerOverlay} {...motion.overlay} style={{ position: 'absolute', left: 0, right: 0, bottom: 0, display: 'flex', justifyContent: 'center', paddingLeft: gutter, paddingRight: gutter, paddingBottom: 16 }}>
        {/* Pending approvals and questions stack on the composer; a blocking one replaces it. */}
        <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', width: '100%', maxWidth: COLUMN, position: 'relative' }}>
          {motion.trayGhost}
          {!thread?.subagent && <ThreadRequests />}
          {!blocking && (
            <ErrorBoundary label="the composer" compact>
              {thread?.moved ? <MoveBar thread={thread} {...((thread as { machineId?: string }).machineId ? { machineId: (thread as { machineId?: string }).machineId! } : {})} /> : thread?.subagent ? thread.provider.kind === 'claude-code' ? <NativeSubagentComposer key={thread.id} thread={thread} /> : <SubagentBar thread={thread} /> : <Composer thread={thread ?? listed} placeholderEnter={motion.placeholderEnter} />}
            </ErrorBoundary>
          )}
        </div>
      </div>
      {motion.over}
    </>
  )
}

const ELSEWHERE: Record<Exclude<Page, 'home'> | 'settings', { icon: string; title: string }> = {
  notes: { icon: N.note, title: 'Notes' },
  tasks: { icon: N.tasks, title: 'Tasks' },
  pulls: { icon: N.pullRequest, title: 'Pull requests' },
  usage: { icon: N.usage, title: 'Usage' },
  settings: { icon: N.settings, title: 'Settings' },
}

function ElsewherePage({ page }: { page: Exclude<Page, 'home'> | 'settings' }) {
  const t = useTheme()
  const { icon, title } = ELSEWHERE[page]
  return (
    <div testId={`page-${page}`} style={{ display: 'flex', flexDirection: 'column', flexGrow: 1, alignItems: 'center', justifyContent: 'center', gap: 10 }}>
      <Icon source={icon} size={28} color={t.iconTertiary} />
      <Label color={t.text} size={15}>
        {title}
      </Label>
      <Label color={t.textSecondary}>{`Not in the GPUIX build yet. Open the desktop app to use ${title}.`}</Label>
    </div>
  )
}
