import { clipboardAttachments, type ClipboardImage } from './composer/clipboard.ts'
import { StatusGlyph } from './composer/StatusGlyph.tsx'
import { FooterRow, useFooterFit } from './composer/footerWidth.tsx'
import { TokenTextarea, type TokenTextareaHandle } from './composer/TokenTextarea.tsx'
import type { TokenReference } from './composer/token-text.ts'
import { type InputSelection } from './native/api.ts'
export { useComposerOverlayHeight, useMeasureComposerOverlay } from './composer/height.ts'
import { useMeasureComposerOverlay } from './composer/height.ts'
import { coordinatorOpenParams, openingMessageText } from '../../../packages/kybern-client/src/coordinator.ts'
import { createdThreadId, optimisticThread, uuidv7, type NewChatCreate } from '../../../packages/kybern-client/src/newChat.ts'
import { newChats, optimisticCreates, useNewChat } from './composer/newChats.ts'
import { machineName } from './composer/outbox.ts'
import { countComposerSend } from './shell/starPromptApp.ts'
import { getNav } from './nav/store.ts'
import { removePending } from './composer/pending.ts'
import { goPage, openThread } from './nav/history.ts'
import { setNav } from './nav/store.ts'
import { N } from './nav/glyphs.ts'
import { activeEnvironment } from './shell/environment.ts'
import { Tip } from './nav/overlay.tsx'
import { load as loadEnvironmentPreference, save as saveEnvironmentPreference } from './shell/persist.ts'
// The composer (`views/Composer.tsx`, `composerPickerStyles.ts`): a 19.2px-radius
// card with the editor on top and a footer of 28px chips (+ menu, permission
// mode, model and effort), then the 32px round send button, which becomes the
// stop control while the thread runs. Queued follow-ups live in the composer tray above
// the card (`requests/ComposerTray.tsx`).
//
// `ThreadComposer` and `DraftComposer` wire it the way `views/Thread.tsx` and
// `views/Draft.tsx` do; the pickers and data live in `src/composer/`.
import { useGpuixRequired, type PublicInstance } from '@gpuix/react'
import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { formatEffort, PROVIDER_LABEL } from '@/lib/format'
import { createComposerMentionReference, nextAttachmentLabel, type AttachmentReference } from '../../../packages/kybern-client/src/composerTokens.ts'
import { findModel, modelQualifier, selectedVariant, selectorEffort, triggerTraits, variantSelector } from '../../../packages/kybern-client/src/models.ts'
import { promptText } from '../../../packages/kybern-client/src/prompts.ts'
import {
  FREE_CHAT_PROJECT_ID,
  isFreeChatProject,
  type ContentPart,
  type MachineId,
  type PermissionMode,
  type ProjectId,
  type ProviderInstance,
  type ProviderKind,
  type ProviderStatus,
  type TaskItem,
  type Thread,
  type UserMessage,
} from '../../../packages/kybern-client/src/types.ts'
import { Icon } from './icons.tsx'
import { MatrixLoader } from './MatrixLoader.tsx'
import { Spinner } from './kit/Spinner.tsx'
import { upsert as upsertTask } from './tasks/store.ts'
import { localClient, threadClient, ownProjectId, type StateThread, getState, refreshLists, setThreadOpen, usePaneCompact, focusPane, getFocusedPaneId, usePaneAccess, getPane, selectPaneThread, setPane, set, useApp } from './state.ts'
import { useCurrentSpaceId } from './spaces/current.ts'
import { FONT_UI } from './theme.ts'
import { ink, Label, REDUCED_MOTION, useTheme } from './ui.tsx'
import { GlassEdge } from './kit/GlassEdge.tsx'
import { frostBackdrop, frostFill, raisedFill, useGlass } from './platform/glass.ts'
import { isPrimary, isPrimaryChord, shortcutLabel } from './platform/keys.ts'
import { addWindowKeyHandler } from './nav/keys.ts'
import { keyboardAnswer } from './requests/Panel.tsx'
import { isUserInput, connectorApproval } from '../../../packages/kybern-client/src/userInput.ts'
import { ANSWER_COPY } from '../../../packages/kybern-client/src/visualAnswer.ts'
import { isVisualAnswerCard, pendingVisualAnswer, replyToVisual } from './requests/VisualAnswer.tsx'
import { setThreadComposerFocus } from './composer/focus.ts'
import { markLaunchEnter } from './draft/store.ts'
import { handleCardKey, isRowCard } from './requests/actions.ts'
import { cardKeyHandler, focusCard, setComposerEmpty } from './requests/ui.ts'
import { noteKeystroke, openHeldCard } from './requests/typingGuard.ts'
import { createDraftStore } from './composer/drafts.ts'
import { kitOverlayOpen } from './kit/AlertDialog.tsx'
import { useControls } from './kit/controls.ts'
import { IconButton } from './kit/IconButton.tsx'
import { PickerChip } from './kit/PickerChip.tsx'
import { G } from './composer/glyphs.ts'
import { muted, PickerItem, PickerLabel, PickerMenu } from './composer/Menu.tsx'
import { clipboardSources, sendTargets } from '../../../packages/kybern-client/src/machineUtilities.ts'
import { glyphOfMachine, useMachines } from './machines/index.ts'
import { readClipboardFrom, sendClipboardTo } from './machines/clipboard.ts'
import { MachineBadge } from './sidebar/MachineBadge.tsx'
import { ThreadMachine } from './machines/ThreadMachine.tsx'
import { useCompletion } from './composer/Completion.tsx'
import { findSecret, scanInsert, type SecretMatch } from './composer/secretCatcher.ts'
import { isSummonedCard } from './shell/waitingLine.ts'
import { SecretDetected, SecretNameForm, type SecretWorkspace, type StoredSecret } from './composer/SecretCatch.tsx'
import { secretMentionPart, type MentionPart } from '../../../packages/kybern-client/src/userInput.ts'
import { PanelEnter, PanelPresence } from './requests/Panel.tsx'
import { useThreadCommands } from './composer/commands.ts'
import { tokenIcon } from './composer/token-icons.ts'
import { accountLead, ModelPicker, ModelTriggerLabel } from './composer/ModelPicker.tsx'
import { PermissionPicker } from './composer/PermissionPicker.tsx'
import { onlyFullAccess } from '../../../packages/kybern-client/src/permissionModes.ts'
import { MachinePicker } from './composer/MachinePicker.tsx'
import { chooseMachine, useMachineLists, useMachineTarget } from './composer/machineTarget.ts'
import { groupPathsKey } from './composer/groupPaths.ts'
import { Leave, Pop, SwapOnChange, useLeavingList } from './composer/motion.tsx'
import { Rise } from './motion/kit.tsx'
import { useNativeWindowSize } from './native/layout-hooks.ts'
import { Presence } from './motion/Presence.tsx'
import { DropHighlight } from './motion/DropHighlight.tsx'
import { ImageViewer, type MorphSource } from './motion/ImageMorph.tsx'
import { PATCHED_NATIVE } from './native/local-build.ts'
import { usePendingSends } from './composer/pending.ts'
import type { LayoutRect } from './native/layout.ts'
import type { PaintTransformProps } from '@gpuix/native/host'

import { hoverPressTransform } from './motion/press.ts'
import { describeProblem } from '../../../packages/kybern-client/src/problems.ts'
import {
  launchNewChat,
  ensureLoaded,
  errorText,
  onComposerRestore,
  onComposerInsertChip,
  rememberSentDraft,
  getData,
  interruptThread,
  createOnMachine,
  loadCatalog,
  loadMachineLists,
  loadProviders,
  loadQueue,
  loadTarget,
  patch,
  patchDraft,
  removeQueued,
  sendToThread,
  setThreadMode,
  setThreadModel,
  uploadPath,
  uploadBytes,
  useComposer,
} from './composer/store.ts'
import { alreadyAttachedText, contentHash, newFiles } from './composer/attached.ts'
import { isGroupProject } from '../../../packages/kybern-client/src/groups.ts'
import { canSteer } from '../../../packages/kybern-client/src/steering.ts'
import { choiceFor, groupCreateParams, mappedGroupChoice } from './groups/tray.ts'

export const COMPOSER_RADIUS = 19.2
const DEFAULT_PLACEHOLDER = 'Ask anything, @ to mention, $ skills, or / commands'

export function Composer({ thread, tray, placeholderEnter }: { thread: Thread | null; tray?: ReactNode; placeholderEnter?: PaintTransformProps }) {
  const status = useApp((s) => s.status)
  useEffect(() => {
    if (status === 'open') ensureLoaded()
  }, [status])
  // The open transcript folds `thread_updated` events, so its copy carries the
  // live status; the thread list only refreshes on `projects`/list changes.
  const live = useApp((s) => (!thread ? null : s.open.thread?.id === thread.id ? s.open.thread : s.threads.find((th) => th.id === thread.id) ?? thread))
  return live ? <ThreadComposer key={live.id} thread={live} placeholderEnter={placeholderEnter} /> : <DraftComposer tray={tray} />
}

// ---- wiring: an open thread ----

function ThreadComposer({ thread, placeholderEnter }: { thread: StateThread; placeholderEnter?: PaintTransformProps }) {
  const pane = usePaneAccess()
  const threadId = thread.id
  // A send the daemon has not answered yet already counts as the running turn: the next Enter queues behind it.
  const starting = usePendingSends(threadId).some((p) => p.delivery === 'send' && p.state === 'sending')
  const running = thread.status === 'running' || thread.status === 'awaiting-approval' || starting
  const target = useComposer((d) => (d.target?.threadId === threadId ? d.target.state : null))
  // The agents and accounts of the machine that owns the thread (the machine you are at, unless the thread is another's).
  const { providers, accounts } = useMachineLists(thread.machineId)
  // A new chat still waiting in the outbox has no thread to send into yet.
  const waitingChat = useNewChat(threadId)?.state === 'waiting'
  const catalog = useComposer((d) => d.catalog)
  const catalogLoading = useComposer((d) => d.catalogLoading)
  const approval = useApp((s) => (s.open.thread?.id === threadId ? s.open.pendingApprovals[0] : null))
  const projectName = useApp(s => s.projects.find(p => p.id === thread.coordinator_project_id)?.name)

  useEffect(() => {
    if (!pane.frozen) void loadTarget(threadId, thread.machineId)
  }, [threadId, thread.machineId, thread.status, thread.provider.instance, thread.model, pane.frozen])

  const provider = target?.target.provider ?? thread.provider
  const catalogKey = `${threadId}:${provider.kind}:${provider.instance}`
  useEffect(() => {
    if (!pane.frozen) void loadCatalog(threadId, provider, ownProjectId(thread), false, thread.machineId).catch(() => {})
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [catalogKey, thread.project_id, pane.frozen])
  const composerProviders = useMemo(
    () => providers.map((status) => (status.kind === provider.kind && catalog?.key === catalogKey ? catalog.status : status)),
    [providers, provider.kind, catalog, catalogKey],
  )
  const targetChanged = !!target && (target.target.provider.kind !== thread.provider.kind || target.target.provider.instance !== thread.provider.instance)
  const mode = targetChanged ? target?.pending_permission_mode ?? thread.permission_mode : thread.permission_mode
  const coordinator = !!thread.coordinator_project_id
  const handoffSelected =
    !!target && (targetChanged || (target.target.model ?? null) !== (thread.model ?? null) || (target.target.effort ?? null) !== (thread.effort ?? null))
  const chooseTarget = async (next: ProviderInstance, model?: string, effort?: string, inherit = !target?.account_override) => {
    const state = await threadClient(threadId, thread.machineId)!.call('threads.target.set', { thread_id: threadId, target: { provider: next, model, effort }, inherit_account: inherit })
    patch({ target: { threadId, state } })
  }

  const switchHarness = async (next: ProviderInstance, model?: string, effort?: string) => {
    if (running) throw new Error("Finish or stop the Coordinator's current turn before changing its harness.")
    const available = providers.find(p => p.kind === next.kind && p.available)
    if (!available) throw new Error('Choose an available harness for the Coordinator.')
    const modes = available.supported_permission_modes
    const permission = modes.includes(thread.permission_mode) ? thread.permission_mode : modes.includes('supervised') ? 'supervised' : modes[0]
    if (!permission) throw new Error('This harness does not expose a supported permission mode.')
    const owner = activeEnvironment()
    const switched = await threadClient(threadId, thread.machineId)!.call('coordinator.switch_harness', { project_id: thread.coordinator_project_id!, provider: next, model, effort, permission_mode: permission })
    if (owner !== activeEnvironment()) return
    const state = pane.get()
    set({ threads: state.threads.map(t => t.id === switched.id ? switched : t) })
    if (state.open.thread?.id === switched.id) setThreadOpen(switched.id, { ...state.open, thread: switched })
    await loadTarget(switched.id)
  }

  return (
    <ComposerView
      draftKey={`thread:${threadId}`}
      focusRef={(focus) => setThreadComposerFocus(pane.id, focus)}
      projectId={isFreeChatProject(thread.project_id) ? undefined : thread.project_id}
      currentThreadId={threadId}
      thread={thread}
      placeholder={
        approval
          ? isVisualAnswerCard(approval) ? ANSWER_COPY.placeholder
          : isUserInput(approval) && !connectorApproval(approval) ? 'Answer the questions above'
          : isSummonedCard(approval) ? 'Choose above to continue' : 'Resolve this approval request to continue'
          : running
            ? 'Ask for follow-up changes'
            : coordinator
              ? `What should we work on in ${projectName ?? 'this project'}?`
              : undefined
      }
      running={running}
      starting={starting && thread.status !== 'running' && thread.status !== 'awaiting-approval'}
      onStop={() => void interruptThread(threadId).catch(() => {})}
      onSend={(message, delivery) => {
        // A message typed while a visual waits for an answer answers it in words.
        const waiting = pendingVisualAnswer(threadId)
        return (waiting && replyToVisual(waiting, message)) || sendToThread(threadId, message, delivery)
      }}
      sendDisabled={waitingChat}
      sendDisabledReason={waitingChat ? `Waiting for ${machineName(thread.machineId)}. You can send again once it has the chat.` : undefined}
      canSteer={handoffSelected || canSteer(thread.provider)}
      mode={mode}
      onModeChange={(m) => void setThreadMode(threadId, m, thread.machineId).catch((e) => console.error('Unable to change mode:', errorText(e)))}
      provider={provider}
      providers={composerProviders}
      accounts={accounts}
      accountFollowsDefaults={target ? !target.account_override : undefined}
      onAccountChange={coordinator ? running ? undefined : (instance) => switchHarness({ ...provider, instance: instance ?? 'default' }, thread.model ?? undefined, thread.effort ?? undefined) : (instance) => chooseTarget({ ...provider, instance: instance ?? 'default' }, target?.target.model ?? undefined, target?.target.effort ?? undefined, instance === null)}
      model={target?.target.model ?? thread.model ?? undefined}
      effort={target?.target.effort ?? thread.effort ?? undefined}
      onModelChange={coordinator ? running ? undefined : (model, effort) => switchHarness(provider, model, effort) : (model, effort) => setThreadModel(threadId, model, effort, thread.machineId)}
      onProviderChange={coordinator ? running ? undefined : (next, choice) => switchHarness(next, choice?.model, choice?.effort) : (next, choice) => chooseTarget(next, choice?.model, choice?.effort, !choice?.pinAccount)}
      catalogLoading={catalogLoading === catalogKey}
      onReloadModels={(force) => loadCatalog(threadId, provider, ownProjectId(thread), force, thread.machineId).then(() => {})}
      placeholderEnter={placeholderEnter}
      machinePicker={thread.machineId ? <ThreadMachine machineId={thread.machineId} testId="composer-machine" /> : undefined}
    />
  )
}

// ---- wiring: the home screen ----

/** The draft's agent, as `views/Draft.tsx:55-66` picks it. */
export function useDraftTarget(stayLocal = false) {
  const draft = useComposer((d) => d.draft)
  const projects = useApp((s) => s.projects)
  // The most recently active thread's project, or the first project.
  const spaceId = useCurrentSpaceId()
  const recentProject = useApp((s) => {
    // Runs on every store update (streaming included): one pass over each list.
    const here = s.projects.filter((p) => !isFreeChatProject(p.id) && (spaceId === null || p.space_id === spaceId))
    const ids = new Set(here.map((p) => p.id))
    return s.threads.find((th) => ids.has(th.project_id))?.project_id ?? here[0]?.id
  })
  const entry = useApp(s => s.draft)
  const projectId = entry.projectId ? entry.projectId : draft.projectId === undefined ? recentProject ?? null : draft.projectId
  const listed = projectId ? projects.find((p) => p.id === projectId) : undefined
  // Where the chat runs (`composer/machineTarget.ts`): with one machine this is the machine you are at, at no cost. A project that
  // only another machine has runs there. A Coordinator or a task's run stays on the machine you are at.
  const machine = useMachineTarget(projectId ?? null, !stayLocal)
  const { providers, settings, accounts, loading: listsLoading, loaded: listsLoaded } = useMachineLists(machine.machineId)
  // The checkout the lists follow: the chosen machine's own (its path keys that machine's project accounts).
  const project = machine.remote && machine.chosen?.project ? machine.chosen.project : listed
  const available = useMemo(() => providers.filter((p) => p.available), [providers])
  const provider = useMemo<ProviderInstance | null>(() => {
    const stored = draft.provider
    const instanceFor = (kind: ProviderKind) => {
      const config = settings?.providers[kind]
      return (project ? config?.project_accounts?.[project.path] : undefined) ?? config?.default_account ?? 'default'
    }
    if (stored && available.some((p) => p.kind === stored.kind)) {
      if ((stored.pinAccount || stored.instance !== 'default') && (!machine.remote || stored.instance === 'default' || accounts.some((account) => account.provider.kind === stored.kind && account.provider.instance === stored.instance))) return stored
      return { kind: stored.kind, instance: instanceFor(stored.kind) }
    }
    const pick = available.find((p) => p.kind === settings?.default_provider) ?? available[0]
    return pick ? { kind: pick.kind, instance: instanceFor(pick.kind) } : null
  }, [draft.provider, available, settings, project, accounts, machine.remote])
  const pinAccount = !!draft.provider && provider?.kind === draft.provider.kind && provider?.instance === draft.provider.instance && (draft.provider.pinAccount ?? draft.provider.instance !== 'default')
  const preferred = draft.mode ?? settings?.default_permission_mode ?? 'supervised'
  const status = provider ? available.find((p) => p.kind === provider.kind) : undefined
  const mode: PermissionMode = onlyFullAccess(status ?? provider?.kind) ? 'full-access' : status && !status.supported_permission_modes.includes(preferred) ? status.supported_permission_modes[0] ?? preferred : preferred
  const useWorktree = !projectId ? false : draft.worktree ?? project?.worktrees_default ?? settings?.worktrees_default ?? false
  return { draft, projectId, project, listed, machine, providers, accounts, listsLoading, listsLoaded, provider, pinAccount, mode, status, useWorktree, choice: provider ? draft.models[provider.kind] : undefined }
}

/** What the new chat screen (`Draft.tsx`) adds to the draft composer: an attached Up next task, the pick-up's keyboard entry
 *  and the launch record for the send transition. */
export interface DraftExtras {
  /** Sends through `tasks.items.send` (Send to agent) instead of `threads.create`; Send works with nothing typed. */
  task?: TaskItem | null
  placeholder?: string
  onEmptyDown?: () => boolean
  focusRef?: (focus: () => void) => void
  /** The send has a thread id: record where the prompt was, before the page switches. */
  onLaunch?: (threadId: string, snapshot: ComposerSnapshot | undefined) => void
  /** The send started the task's run. */
  onTaskSent?: () => void
}

export function DraftComposer({ tray, extras = {} }: { tray?: ReactNode; extras?: DraftExtras }) {
  const pane = usePaneAccess()
  // The new chat mounts this directly (not through `Composer`), so it loads the agents itself.
  const status = useApp((s) => s.status)
  useEffect(() => {
    if (status === 'open') ensureLoaded()
  }, [status])
  const coordinator = useApp(s => s.draft.purpose === 'coordinator')
  const { projectId, project, machine, providers, accounts, listsLoading: loading, provider, pinAccount, mode, useWorktree, choice, draft } = useDraftTarget(coordinator || !!extras.task)
  // An unedited group spans every target member without needing to translate any source paths, even offline.
  const groupNeedsMapping = machine.groupPathsReady === false && draft.group?.projectId === projectId &&
    (draft.group.members !== null || Object.keys(draft.group.baseBranches).length > 0)

  return (
    <ComposerView
      key={`${coordinator ? 'coordinator' : 'project'}:${projectId ?? 'free'}`}
      draftKey={`${coordinator ? 'coordinator' : 'project'}:${projectId ?? 'free'}`}
      placeholder={extras.placeholder ?? (coordinator ? 'Describe the outcome, constraints, and what success looks like' : undefined)}
      projectId={projectId ?? undefined}
      above={tray}
      launch
      allowEmptySend={!!extras.task}
      onEmptyDown={extras.onEmptyDown}
      focusRef={extras.focusRef}
      mode={mode}
      onModeChange={(m) => patchDraft({ mode: m }, pane.id)}
      provider={provider}
      providers={providers}
      accounts={accounts}
      model={choice?.model}
      effort={choice?.effort}
      onModelChange={async (model, effort) => {
        if (provider) patchDraft({ models: { ...getData(pane.id).draft.models, [provider.kind]: { model, effort } } }, pane.id)
      }}
      onProviderChange={async (next, nextChoice) => {
        const models = nextChoice ? { ...getData(pane.id).draft.models, [next.kind]: nextChoice } : getData(pane.id).draft.models
        const nextStatus = providers.find((item) => item.kind === next.kind)
        // A Full-access-only harness (Antigravity) gets Full access from the derived mode; the draft keeps its own choice for switching back.
        const keepMode = !nextStatus || onlyFullAccess(nextStatus) || nextStatus.supported_permission_modes.includes(mode)
        patchDraft({
          provider: { ...next, pinAccount: nextChoice?.pinAccount ?? next.instance !== 'default' },
          models,
          ...(keepMode ? {} : { mode: nextStatus!.supported_permission_modes.includes('supervised') ? 'supervised' : nextStatus!.supported_permission_modes[0] ?? mode }),
        }, pane.id)
      }}
      catalogLoading={loading}
      onReloadModels={(force) => machine.machineId ? loadMachineLists(machine.machineId, force, machine.chosen?.projectId) : loadProviders(force, projectId ?? undefined).then(() => {})}
      machinePicker={machine.show ? <MachinePicker target={machine} /> : undefined}
      uploadMachine={machine.remote && machine.reachable ? machine.machineId : undefined}
      agentsOn={machine.remote ? machine.chosen?.name : undefined}
      sendDisabled={!provider || groupNeedsMapping || (coordinator && !projectId)}
      sendDisabledReason={groupNeedsMapping ? machine.groupPathsError ?? 'Matching repositories on the selected machine…' : !provider ? machine.remote && !machine.reachable ? `${machine.chosen?.name} is offline and its agents have not been seen yet.` : 'Choose an available agent before sending.' : coordinator && !projectId ? 'Choose a project for the Coordinator.' : undefined}
      onSend={async (message, _delivery, snapshot) => {
        if (!provider) return
        // A new chat is made on the machine the user is at, or on the one the machine chip names (`composer/machineTarget.ts`).
        const target = machine.remote && !coordinator && !extras.task ? machine.chosen : undefined
        const client = localClient()
        if (!client) throw new Error('Reconnect and try again.')
        if (machine.show && machine.chosen) chooseMachine(machine.key, machine.chosen.machineId, pane.id)
        // Enter starts the chat on this frame (`composer/newChats.ts`): the thread id is ours, the page switches and the
        // prompt flies into its bubble while the daemon makes the thread behind it.
        const threadId = uuidv7()
        const owner = activeEnvironment()
        const live = () => localClient() === client && owner === activeEnvironment()
        // A daemon before 0.15.0 ignores the client's ids and names the thread itself: it gets the awaited create.
        // Another machine honours the ids too (every machine in a network is new enough to be in it).
        const optimistic = target ? true : optimisticCreates(getNav().version)
        const ids = (messageId: string) => (optimistic ? { thread_id: threadId, message_id: messageId } : {})
        const launch = async (thread: Thread, worktree: boolean, create: NewChatCreate, sent: UserMessage = message) => {
          const draftKey = `${pane.id}:${coordinator ? 'coordinator' : 'project'}:${projectId ?? 'free'}`
          if (!optimistic) {
            const id = createdThreadId(await create(sent, crypto.randomUUID()))
            drafts.drop(draftKey)
            if (!id || !live()) return
            extras.onLaunch?.(id, snapshot)
            await refreshLists()
            if (live()) void selectPaneThread(pane.id, id)
            return
          }
          // The prompt now belongs to the thread: drop the saved draft before the page switches, so no composer reloads it.
          drafts.drop(draftKey)
          extras.onLaunch?.(threadId, snapshot)
          launchNewChat({ paneId: pane.id, threadId, thread, message: sent, worktree, create, ...(target ? { machine: { id: target.machineId, name: target.name } } : {}) })
        }
        const account = pinAccount ? provider : { ...provider, instance: 'default' }
        const base = { id: threadId, provider: account, model: choice?.model, effort: choice?.effort, permissionMode: mode, message }
        const task = extras.task
        if (task && !coordinator) {
          // `RunComposer`'s single-task send: the thread is recorded as the task's next run and the task becomes running.
          if (!projectId) throw new Error('Choose a project to run this task in.')
          // The task leads the message as its chip, as the daemon would add it, so the first bubble shows it at once.
          const pending = task.pending_followup?.trim()
          const chip = { type: 'mention' as const, name: task.key, path: `kybern://task/${task.id}`, display_name: `${task.key} ${task.title}` }
          const sent = { ...message, parts: [chip, ...(pending ? [{ type: 'text' as const, text: pending }] : []), ...message.parts] }
          await launch(optimisticThread({ ...base, projectId, title: task.title }), useWorktree, async (first, messageId) => {
            const result = await client.call('tasks.items.send', {
              id: task.id,
              provider: account,
              model: choice?.model,
              effort: choice?.effort,
              permission_mode: mode,
              use_worktree: useWorktree,
              base_branch: useWorktree ? draft.baseBranch ?? null : null,
              project_id: projectId,
              message: first,
              note_ids: [...new Set([...task.note_ids, ...(task.source_note_id ? [task.source_note_id] : [])])],
              ...ids(messageId),
            })
            if (live()) {
              upsertTask(result.task)
              extras.onTaskSent?.()
            }
            return result.thread_id
          }, sent)
          return
        }
        if (coordinator) {
          const available = providers.find(p => p.kind === provider.kind && p.available)
          if (!projectId || !available) throw new Error('Choose a project and an available harness.')
          // A Coordinator plans the attached task rather than running it: the task goes in as a mention, which the daemon
          // expands for the provider.
          const lead = task ? [{ type: 'mention' as const, name: task.key, path: `kybern://task/${task.id}`, display_name: `${task.key} ${task.title}` }] : []
          const sent = lead.length ? { ...message, parts: [...lead, ...message.parts] } : message
          // The project has its Coordinator already: the message is an ordinary send to it.
          const existing = getState().threads.find(t => t.coordinator_project_id === projectId)
          if (existing) {
            drafts.drop(`${pane.id}:coordinator:${projectId}`)
            void selectPaneThread(pane.id, existing.id)
            await sendToThread(existing.id, sent, 'send')
            if (task) extras.onTaskSent?.()
            return
          }
          // `coordinator.open` takes the opening words as text: an image alone would never reach it.
          if (!openingMessageText(sent)) throw new Error('Tell the Coordinator about the project first.')
          const params = coordinatorOpenParams({ projectId, provider: available, instance: provider.instance, model: choice?.model, effort: choice?.effort, permissionMode: mode, openingMessage: openingMessageText(sent) })
          const projectName = getState().projects.find(p => p.id === projectId)?.name ?? 'Project'
          await launch(optimisticThread({ ...base, provider: params.provider, projectId, message: sent, title: `${projectName} coordinator`, coordinatorProjectId: projectId }), false, async (first, messageId) => {
            const result = await client.call('coordinator.open', { ...params, opening_message: openingMessageText(first), ...ids(messageId) })
            if (!optimistic || result.thread.id === threadId) { if (task && live()) extras.onTaskSent?.(); return result.thread.id }
            // Another client opened this project's Coordinator first: the message goes to that one, and the page follows it.
            await client.call('threads.send', { thread_id: result.thread.id, message: first, message_id: messageId })
            newChats.forget(threadId)
            removePending(threadId, messageId)
            if (task && live()) extras.onTaskSent?.()
            if (live() && getPane(pane.id)?.selectedId === threadId) void selectPaneThread(pane.id, result.thread.id)
          }, sent)
          return
        }
        const freeChat = !projectId
        if (target) {
          // The chat is made on another machine; if that machine is away the create waits in the outbox ("Waiting for satyamas").
          const thread: StateThread = {
            ...optimisticThread({ ...base, projectId: projectId ?? FREE_CHAT_PROJECT_ID }),
            machineId: target.machineId,
            ...(target.projectId ? { ownProjectId: target.projectId } : {}),
            ...(target.state === 'online' ? {} : { offline: true }),
          }
          await launch(thread, useWorktree && !freeChat, (first, messageId) => createOnMachine(target, {
            ...(target.projectId ? { project_id: target.projectId } : {}),
            provider: account,
            pin_account: pinAccount,
            permission_mode: mode,
            model: choice?.model,
            effort: choice?.effort,
            use_worktree: useWorktree && !freeChat,
            ...(draft.baseBranch ? { base_branch: draft.baseBranch } : {}),
            ...(project && isGroupProject(project) ? groupCreateParams(project, mappedGroupChoice(draft.group, projectId!, draft.groupPaths?.[groupPathsKey(machine.key, projectId!, target.machineId)]), useWorktree) : {}),
            message: first,
            thread_id: threadId,
            message_id: messageId,
          }))
          return
        }
        await launch(optimisticThread({ ...base, projectId: projectId ?? FREE_CHAT_PROJECT_ID }), useWorktree && !freeChat, async (first, messageId) => {
          return await client.call('threads.create', {
            ...(projectId ? { project_id: projectId } : {}),
            provider: account,
            pin_account: pinAccount,
            permission_mode: mode,
            model: choice?.model,
            effort: choice?.effort,
            use_worktree: useWorktree,
            base_branch: draft.baseBranch ?? undefined,
            // A group project: the repositories this chat gets and their branches (`groups/tray.ts`), next to the client ids.
            ...(project && isGroupProject(project) ? groupCreateParams(project, choiceFor(draft.group, project.id), useWorktree) : {}),
            message: first,
            ...ids(messageId),
          })
        })
      }}
    />
  )
}

// ---- the shell ----

interface Attachment {
  id: string
  name: string
  media_type: string
  size: number
  /** Inline label (`image1`) the prompt can mention as `@image1`. */
  label: string
  path: string
  /** A pasted image's content digest, so pasting the same picture twice attaches it once (`composer/attached.ts`). */
  hash?: string
}

/**
 * A model id the catalog has not listed yet (it loads a few seconds after
 * launch): `claude-fable-5-1` reads "Claude Fable 5.1", as the desktop's
 * fallback comment in `views/Composer.tsx` intends; other ids show as they are.
 */
function uncataloguedName(model: string): string {
  const claude = model.match(/^claude-([a-z]+)-(\d+)(?:-(\d{1,2}))?(?:-\d{6,})?$/)
  if (!claude) return model
  return `Claude ${claude[1]![0]!.toUpperCase()}${claude[1]!.slice(1)} ${claude[2]}${claude[3] ? `.${claude[3]}` : ''}`
}

/** Text and attachments kept per thread or project while the composer is away (`composer/drafts.ts`). */
const drafts = createDraftStore<Attachment>(() => loadEnvironmentPreference<[string, { text: string; attachments: Attachment[] }][]>('kybern.composer.drafts', []), (entries) => saveEnvironmentPreference('kybern.composer.drafts', entries))

/** Pasted keys the person chose to keep as text (this session, in memory only): they are not flagged again. */
const keptSecrets = new Set<string>()

export interface ComposerViewProps {
  draftKey: string
  /** Enables @ files and the skill catalog (`views/Composer.tsx` `projectId`). */
  projectId?: ProjectId
  currentThreadId?: string
  /** The open thread: its slash commands (`composer/commands.ts`) replace the home screen's. */
  thread?: Thread
  placeholder?: string
  running?: boolean
  /** `running` only because a send has not been answered yet: Enter still queues behind it, but the button shows that send
   *  in flight, not Stop (there is no turn to stop yet). */
  starting?: boolean
  onStop?: () => void
  onSend: (message: UserMessage, delivery: 'send' | 'queue' | 'steer', snapshot?: ComposerSnapshot) => Promise<void>
  canSteer?: boolean
  sendDisabled?: boolean
  sendDisabledReason?: string
  readOnly?: boolean
  lockMode?: boolean
  mode: PermissionMode
  onModeChange: (mode: PermissionMode) => void
  provider: ProviderInstance | null
  providers: ProviderStatus[]
  accounts: import('../../../packages/kybern-client/src/types.ts').AccountSummary[]
  accountFollowsDefaults?: boolean
  onAccountChange?: (instance: string | null) => Promise<void>
  model?: string | null
  effort?: string | null
  onModelChange?: (model: string | undefined, effort: string | undefined) => Promise<void>
  onProviderChange?: (provider: ProviderInstance, choice?: { model?: string; effort?: string; pinAccount?: boolean }) => Promise<void>
  catalogLoading: boolean
  onReloadModels: (force: boolean) => Promise<void>
  above?: ReactNode
  /** The draft (text and attachments) became empty or non-empty; the task dock collapses an empty one when the page is clicked. */
  onEmptyChange?: (empty: boolean) => void
  /** The new chat: the prompt and its attachments stay in place (read-only) until the send resolves, so the thread page can
   *  carry them away (`draft/Morph.tsx`), and `onSend` receives where they were on screen. */
  launch?: boolean
  /** Send is enabled with nothing typed (a task is attached: the run starts from the task). */
  allowEmptySend?: boolean
  /** ↓ in an empty editor with no completion open; true when it moved focus elsewhere (the new chat's pick-up). */
  onEmptyDown?: () => boolean
  /** Receives a function that focuses the editor (focus comes back from the pick-up). */
  focusRef?: (focus: () => void) => void
  /** The placeholder's entrance (the thread composer holds it while a prompt flies off the card). */
  placeholderEnter?: PaintTransformProps
  /** The machine chip of a new chat (`composer/MachinePicker.tsx`), before the agent and model chips. */
  machinePicker?: ReactNode
  /** Attachments upload to this machine's assets (a new chat that runs there); unset is the machine the user is at. */
  uploadMachine?: MachineId
  /** The machine a new chat runs on when it is not this one: the agent picker lists that machine's agents and sign-ins. */
  agentsOn?: string
}

/** Where the prompt was when Send was pressed (window coordinates; `ComposerViewProps.launch`). */
export interface ComposerSnapshot {
  card: LayoutRect | null
  editor: LayoutRect | null
  text: string
  /** Visual lines in the editor. */
  lines: number
  /** The editor had more lines than it shows. */
  scrolled: boolean
  images: { id: string; src: string; rect: LayoutRect }[]
}

const MIN_ROWS = 2
const LINE_HEIGHT = 19.6
/** Share of the window height the text field may take (`max-h-[40vh]`). */
const MAX_FIELD_SHARE = 0.4

export function ComposerView(input: ComposerViewProps) {
  const compact = usePaneCompact()
  // The field stops growing at 40% of the window and scrolls inside beyond that, so a long draft never covers the thread.
  const maxRows = Math.max(MIN_ROWS, Math.floor((useNativeWindowSize().height * MAX_FIELD_SHARE) / LINE_HEIGHT))
  const pane = usePaneAccess()
  const props = { ...input, draftKey: `${pane.id}:${input.draftKey}`, readOnly: input.readOnly || pane.frozen }
  const measureOverlay = useMeasureComposerOverlay()
  const t = useTheme()
  const g = useGlass()
  const renderer = useGpuixRequired()
  const { running, provider, providers, model, effort, mode } = props
  const connected = useApp((s) => s.status === 'open')
  const saved = drafts.get(props.draftKey)
  const [text, setText] = useState(saved?.text ?? '')
  const [attachments, setAttachments] = useState<Attachment[]>(saved?.attachments ?? [])
  const [uploading, setUploading] = useState(0)
  // What the composer holds or is uploading, read by the add paths so a repeat is skipped even mid-upload.
  const attachmentsNow = useRef(attachments)
  attachmentsNow.current = attachments
  const pendingPaths = useRef(new Set<string>())
  const pendingHashes = useRef(new Set<string>())
  // A Finder drag over the card (`dragOver` in the desktop composer), eased by `DropHighlight`.
  const [dragOver, setDragOver] = useState(false)
  const [sending, setSending] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [feedback, setFeedback] = useState<string | null>(null)
  const pastePending = useRef(false)
  const machines = useMachines()
  // The pasted-secret catcher (`composer/SecretCatch.tsx`): while it holds a key, nothing is saved as a draft or sent.
  const [secretCatch, setSecretCatch] = useState<{ match: SecretMatch; phase: 'detected' | 'name' } | null>(null)
  const secretCatchRef = useRef(secretCatch)
  secretCatchRef.current = secretCatch
  const textNow = useRef(text)
  const submitLatest = useRef<() => Promise<void>>(async () => {})
  const [promptMode, setPromptMode] = useState<'queue' | 'steer'>('queue')
  const [changingModel, setChangingModel] = useState(false)
  const editor = useRef<PublicInstance | null>(null)
  const tokenEditor = useRef<TokenTextareaHandle>(null)
  const card = useRef<PublicInstance | null>(null)
  const [selection, setSelection] = useState<InputSelection>({ start: text.length, end: text.length, reversed: false, composing: false })
  const readSelection = () => {
    if (!tokenEditor.current) return
    const next = tokenEditor.current.selection()
    setSelection(old => old.start === next.start && old.end === next.end && old.composing === next.composing && old.reversed === next.reversed ? old : next)
  }
  const edit = (start: number, end: number, replacement: string, added?: readonly TokenReference[]) => {
    if (!editor.current || props.readOnly) return false
    try {
      if (!tokenEditor.current?.edit(start, end, replacement, added)) return false
      readSelection()
      focusEditor()
      return true
    } catch (e) { setError(errorText(e)); return false }
  }
  const lastModelRefresh = useRef(0)
  const approvalDigit = useRef<string | null>(null)
  const steering = !!running && !!props.canSteer && promptMode === 'steer'

  useEffect(() => {
    // A caught key is never written to a draft: the text stays out of storage until the person chooses.
    if (secretCatch) return
    drafts.keep(props.draftKey, text, attachments)
  }, [text, attachments, props.draftKey, !!secretCatch])

  // The caught text left the draft (deleted, or edited into something else): the strip goes with it.
  useEffect(() => {
    textNow.current = text
    const current = secretCatchRef.current
    if (!current) return
    const again = findSecret(text)
    if (!again || again.value !== current.match.value) setSecretCatch(null)
    else if (again.start !== current.match.start || again.end !== current.match.end) setSecretCatch({ ...current, match: { ...current.match, start: again.start, end: again.end } })
  }, [text])

  const secretWorkspace = useMemo<SecretWorkspace | null>(
    () => props.currentThreadId ? { thread_id: props.currentThreadId } : props.projectId ? { project_id: props.projectId } : null,
    [props.currentThreadId, props.projectId],
  )
  const keepSecret = (send: boolean) => {
    const current = secretCatchRef.current
    if (!current) return
    keptSecrets.add(current.match.value)
    setSecretCatch(null)
    if (send) setTimeout(() => void submitLatest.current(), 0)
  }
  /** The key is in the file: its run in the text becomes a `$NAME` chip, which is all the agent receives. */
  const secretStored = (stored: StoredSecret) => {
    const current = secretCatchRef.current
    if (!current) return
    const located = findSecret(textNow.current)
    const match = located && located.value === current.match.value ? located : current.match
    const source = textNow.current
    const token = `$${stored.name}`
    const reference = completion.addChip(secretMentionPart({ key: stored.name, file: stored.file }), token)
    const lead = match.start > 0 && !/\s/.test(source[match.start - 1]!) ? ' ' : ''
    const trail = /\s/.test(source[match.end] ?? '') ? '' : ' '
    setSecretCatch(null)
    if (!edit(match.start, match.end, `${lead}${token}${trail}`, [reference])) {
      setText(`${source.slice(0, match.start)}${lead}${token}${trail}${source.slice(match.end)}`)
      focusEditor()
    }
    setFeedback(`Stored ${stored.name} in ${stored.file}${stored.gitignoreUpdated ? ' and added it to .gitignore' : ''}`)
  }

  // Automation: the native file picker cannot be driven, so a test run can name
  // files to attach on mount (`KYBERN_GPUIX_ATTACH=/a.png,/b.pdf`).
  useEffect(() => {
    const preset = process.env.KYBERN_GPUIX_ATTACH
    if (preset && connected) void addFiles(preset.split(','))
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [connected])

  const empty = text.trim().length === 0 && attachments.length === 0
  const tileNodes = useRef(new Map<string, PublicInstance>())
  useEffect(() => { props.onEmptyChange?.(empty) }, [empty])
  useEffect(() => { setComposerEmpty(props.draftKey, empty); return () => setComposerEmpty(props.draftKey, true) }, [props.draftKey, empty])
  const permissionUnsupported = provider?.kind !== 'cursor' && !!providers.find(p => p.kind === provider?.kind && !p.supported_permission_modes.includes(mode))
  const disabledReason = secretCatch ? 'Choose what to do with the key first.' : !connected ? 'Reconnect to send a message.' : props.readOnly ? 'This conversation is read-only.' : props.sendDisabled ? props.sendDisabledReason ?? 'Sending is unavailable.' : permissionUnsupported ? 'Choose a permission mode supported by this agent.' : sending ? 'Sending…' : uploading > 0 ? 'Wait for attachments to finish uploading.' : empty && !props.allowEmptySend ? 'Write a message or attach a file.' : null
  const canSend = !disabledReason

  const focusEditor = () => {
    if (editor.current) renderer.focusElement?.(editor.current.id)
  }
  useEffect(() => { props.focusRef?.(focusEditor) })

  const attachmentRefs = useMemo<AttachmentReference[]>(
    () => attachments.map((a) => ({ token: `@${a.label}`, part: { type: 'attachment', asset_id: a.id, name: a.name, media_type: a.media_type, size: a.size } })),
    [attachments],
  )
  const threadCommands = useThreadCommands(props.thread ?? null, () => void addFiles())
  const completion = useCompletion({
    draftKey: props.draftKey,
    text,
    setText,
    caret: selection.reversed ? selection.start : selection.end,
    composing: selection.composing || selection.start !== selection.end,
    edit,
    projectId: props.projectId,
    provider: provider ?? undefined,
    currentThreadId: props.currentThreadId,
    attachments: attachmentRefs,
    commands: props.thread ? threadCommands : [
      { name: 'resume', hint: 'Continue a saved session', icon: G.clock, run: () => setNav({ sessions: { projectId: props.projectId ?? null } }) },
      { name: 'sessions', hint: 'Browse saved sessions', icon: G.clock, run: () => setNav({ sessions: { projectId: props.projectId ?? null } }) },
      { name: 'attach', hint: 'Attach files or images', icon: G.paperclip, run: () => void addFiles() },
      { name: 'settings', hint: 'Open settings', icon: N.settings, run: () => setNav({ settingsOpen: true }) },
      { name: 'usage', hint: 'Review token usage and cost', icon: N.usage, run: () => goPage('usage') },
    ],
  })

  // Scope approval shortcuts strictly to this editor. Native events cannot
  // prevent text insertion, so consume only the exact digit below.
  useEffect(() => addWindowKeyHandler((e) => {
    const m = e.modifiers
    if (m?.cmd || m?.alt || m?.ctrl || m?.shift) return false
    if (!empty || sending || !props.currentThreadId || !/^([1-9]|escape|up|down)$/.test(e.key ?? '')) return false
    const approval = pane.get().open.pendingApprovals[0]
    if (!approval) return false
    // Last: off macOS the focus query waits on the UI thread, and this runs for every key typed anywhere.
    if (pane.frozen || !editor.current || renderer.getFocusedElementId?.() !== editor.current.id) return false
    // A summoned card (sign-in, permission) routes its own keys; the password prompt registers none, so typing stays typing.
    const custom = approval.thread_id === props.currentThreadId ? cardKeyHandler(approval.id) : undefined
    if (!custom && !isRowCard(approval)) return false
    const route = (key: string) => custom ? custom(key) : handleCardKey(approval, props.currentThreadId!, key)
    // Return is never taken from the composer, so a stray Enter cannot approve anything; ↑ ↓ hand the keyboard to the card.
    if (e.key === 'up' || e.key === 'down') { focusCard(); return route(e.key) }
    keyboardAnswer()
    if (!route(e.key!)) return false
    if (!/^[1-9]$/.test(e.key!)) return true
    approvalDigit.current = e.key!
    // GPUIX 0.10 delivers events asynchronously and cannot prevent native text
    // insertion. Remove only the exact consumed digit, never subsequent typing.
    setTimeout(() => {
      setText((value) => value === e.key ? '' : value)
      approvalDigit.current = null
    }, 0)
    return true
  }), [renderer, completion, empty, sending, props.currentThreadId])

  // A card waiting for a pause (`requests/typingGuard.ts`) opens at once on Esc in the composer; a password or secret never does.
  useEffect(() => addWindowKeyHandler((e) => {
    const m = e.modifiers
    if (e.key !== 'escape' || m?.cmd || m?.alt || m?.ctrl || m?.shift) return false
    // An open `@ $ /` menu takes Esc first: it closes the menu, not the composer.
    if (completion.open || pane.frozen || secretCatchRef.current || kitOverlayOpen() || !editor.current || renderer.getFocusedElementId?.() !== editor.current.id) return false
    const approval = pane.get().open.pendingApprovals[0]
    return !!approval && approval.thread_id === props.currentThreadId && openHeldCard(approval)
  }), [renderer, pane.frozen, props.currentThreadId, completion.open])

  const buildParts = (): ContentPart[] => {
    const parts = completion.buildParts()
    // Attachments the prompt never mentions keep their place at the end.
    const placed = new Set(parts.flatMap((part) => (part.type === 'attachment' ? [part.asset_id] : [])))
    for (const r of attachmentRefs) if (r.part.type === 'attachment' && !placed.has(r.part.asset_id)) parts.push(r.part)
    return parts
  }

  const submit = async (delivery: 'queue' | 'steer' = promptMode) => {
    if (!canSend || secretCatchRef.current || tokenEditor.current?.selection().composing) return
    const status = provider ? providers.find((p) => p.kind === provider.kind) : undefined
    if (status && provider?.kind !== 'cursor' && !status.supported_permission_modes.includes(mode)) {
      setError('Choose a supported permission mode for the selected agent before sending.')
      return
    }
    // The message leaves the composer on this frame; the send runs behind it. Thread sends show at once as a pending
    // bubble and report a rejection there (`composer/pending.ts`); other callers get the draft back with the error.
    const parts = buildParts(), draft = { text, attachments }
    rememberSentDraft(parts, { text, attachments, references: completion.snapshot() })
    // The new chat keeps the prompt where it is until the thread exists; the thread page then carries it off.
    const hold = !!props.launch
    if (hold) markLaunchEnter()
    const snapshot = hold ? snapshotNow() : undefined
    setSending(true)
    setError(null)
    if (!hold) {
      setText('')
      setAttachments([])
      drafts.drop(props.draftKey)
    }
    focusEditor()
    try {
      await props.onSend({ parts }, running ? (delivery === 'steer' && props.canSteer ? 'steer' : 'queue') : 'send', snapshot)
      // A message the person sent, steered or queued: the star prompt's count (`shell/starPrompt.ts`).
      countComposerSend()
      completion.reset()
      if (hold) {
        // The page has usually switched already; clear what this composer saved so the next new chat starts empty.
        drafts.drop(props.draftKey, true)
        setText('')
        setAttachments([])
      }
    } catch (e) {
      // Only into an empty composer: what was typed meanwhile is never overwritten.
      setText((current) => current || draft.text)
      setAttachments((current) => (current.length ? current : draft.attachments))
      // Plain words for what went wrong and what to do; the draft is back in the box, so say so.
      const problem = describeProblem(e, { action: 'send your message' })
      setError(`${problem.title}. ${problem.body} Your message is saved.`)
    } finally {
      setSending(false)
    }
  }
  submitLatest.current = () => submit()
  /** Where the card, the editor and the image tiles are (`launch`); one bounds read each, never per frame. */
  const snapshotNow = (): ComposerSnapshot => {
    const bounds = (node: PublicInstance | null | undefined) => (node ? renderer.getElementBounds?.(node.id) ?? null : null)
    const editorRect = bounds(editor.current)
    const lines = editorRect ? Math.max(1, Math.round(editorRect.height / LINE_HEIGHT)) : 1
    const shown = Math.max(1, text.split('\n').length)
    return {
      card: bounds(card.current),
      editor: editorRect,
      text,
      lines,
      scrolled: lines >= maxRows && shown > lines,
      images: attachments.flatMap((a) => {
        if (!a.media_type.startsWith('image/')) return []
        const rect = bounds(tileNodes.current.get(a.id))
        // The image paints inside the tile's 1 px border.
        return rect ? [{ id: a.id, src: a.path, rect: { x: rect.x + 1, y: rect.y + 1, width: rect.width - 2, height: rect.height - 2 } }] : []
      }),
    }
  }

  useEffect(() => onComposerRestore(props.draftKey, (draft) => {
    // A label the draft's own attachments took meanwhile is renumbered, in the restored text too.
    const taken = attachmentsNow.current.map((a) => a.label), renamed = new Map<string, string>()
    const restored = draft.attachments.filter((a) => !attachmentsNow.current.some((b) => b.id === a.id)).map((a) => {
      if (!taken.includes(a.label)) { taken.push(a.label); return a }
      const label = nextAttachmentLabel(a.media_type, [...taken, ...draft.attachments.map((b) => b.label)])
      taken.push(label); renamed.set(a.label, label)
      return { ...a, label }
    })
    const text = renamed.size ? draft.text.replace(/@([A-Za-z]+\d+)\b/g, (token, label: string) => (renamed.has(label) ? `@${renamed.get(label)}` : token)) : draft.text
    setText((current) => (current.trim() ? `${current}\n\n${text}` : text))
    if (restored.length) setAttachments((list) => [...list, ...restored])
    if (draft.references) completion.restore(draft.references)
    focusEditor()
  }), [props.draftKey])

  // An element ⌥-clicked in the Preview or a visual lands here as a chip at the caret (`pick/hub.ts`).
  const insertChip = (part: MentionPart) => {
    if (props.readOnly) return
    const reference = createComposerMentionReference(part, completion.snapshot().mentions)
    const chip = completion.addChip(part, reference.token)
    const source = textNow.current
    const at = tokenEditor.current?.selection().end ?? source.length
    const lead = at > 0 && !/\s/.test(source[at - 1]!) ? ' ' : ''
    const trail = /\s/.test(source[at] ?? '') ? '' : ' '
    // A composer whose tab is behind another has no laid-out editor to edit: the text state takes the chip instead.
    if (!edit(at, at, `${lead}${reference.token}${trail}`, [chip])) setText(`${source.slice(0, at)}${lead}${reference.token}${trail}${source.slice(at)}`)
  }
  const insertChipLatest = useRef(insertChip)
  insertChipLatest.current = insertChip
  useEffect(() => onComposerInsertChip(props.draftKey, part => insertChipLatest.current(part)), [props.draftKey])

  const addClipboardImage = async (image: ClipboardImage) => {
    const hash = contentHash(image.data)
    if (attachmentsNow.current.some((a) => a.hash === hash) || pendingHashes.current.has(hash)) { setFeedback(alreadyAttachedText([image.name ?? 'This image'])); focusEditor(); return }
    pendingHashes.current.add(hash)
    setUploading((n) => n + 1)
    setError(null)
    const name = image.name ?? 'pasted-image.png'
    try {
      const info = await uploadBytes(name, image.mediaType, image.data, props.currentThreadId, props.uploadMachine)
      // Native images accept data URIs, so previews need no temporary file.
      const path = `data:${image.mediaType};base64,${Buffer.from(image.data).toString('base64')}`
      setAttachments((list) => list.some((a) => a.hash === hash) ? list : [...list, { ...info, path, hash, label: nextAttachmentLabel(info.media_type, list.map((a) => a.label)) }])
    } catch (e) { setError(`Unable to attach image: ${errorText(e)}`) }
    finally { pendingHashes.current.delete(hash); setUploading((n) => n - 1); focusEditor() }
  }

  const addFiles = async (picked?: string[]) => {
    const paths = picked ?? (await renderer.promptForPaths?.({ files: true, multiple: true, prompt: 'Attach' }))
    if (!paths?.length) return
    // A file already attached (or on its way) is not attached again, however its path is spelled (`fileKey`).
    const { fresh, duplicates } = newFiles([...attachmentsNow.current, ...[...pendingPaths.current].map((path) => ({ path }))], paths)
    setFeedback(duplicates.length ? alreadyAttachedText(duplicates.map((path) => path.split('/').pop() || path)) : null)
    if (!fresh.length) { focusEditor(); return }
    for (const path of fresh) pendingPaths.current.add(path)
    setUploading((n) => n + fresh.length)
    for (const path of fresh) {
      try {
        const info = await uploadPath(path, props.currentThreadId, props.uploadMachine)
        // Checked again as it lands: another add may have raced it between this upload and the next render.
        setAttachments((list) => newFiles(list, [path]).fresh.length ? [...list, { ...info, path, label: nextAttachmentLabel(info.media_type, list.map((a) => a.label)) }] : list)
      } catch (e) {
        setError(`Unable to attach ${path.split('/').pop()}: ${errorText(e)}`)
      } finally {
        pendingPaths.current.delete(path)
        setUploading((n) => n - 1)
      }
    }
    focusEditor()
  }

  // ---- model trigger ----
  const status = provider ? providers.find((p) => p.kind === provider.kind) : undefined
  const models = status?.models ?? []
  const current = model ? findModel(models, model) : models.find((m) => m.is_default)
  const traits = triggerTraits(current, model)
  const qualifier = modelQualifier(models, current)
  const effortLabel = effort ?? selectorEffort(current, model) ?? selectedVariant(current, model)?.default_effort ?? current?.default_effort ?? null
  const canPickModel = !!props.onModelChange
  const canReload = canPickModel && !!status?.available && status.supports_model_switch
  const canPickProvider = !!props.onProviderChange
  const busyRun = async (work: () => Promise<void>) => {
    if (changingModel) return false
    setChangingModel(true)
    try {
      await work()
      return true
    } catch (e) {
      setError(`Unable to change model: ${errorText(e)}`)
      return false
    } finally {
      setChangingModel(false)
    }
  }

  const triggerAccount = provider && provider.instance !== 'default' ? props.accounts.find(a => a.provider.kind === provider.kind && a.provider.instance === provider.instance) ?? null : null
  const modelName = current?.display_name ?? (model ? uncataloguedName(model) : provider ? PROVIDER_LABEL[provider.kind] : '')

  return (
    <div ref={measureOverlay} testId="composer-overlay" style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', width: '100%', maxWidth: 736 }}>
      {props.above}
      <PanelPresence show={!!secretCatch}>
        <PanelEnter>
          {secretCatch && (secretCatch.phase === 'detected' || !secretWorkspace
            ? <SecretDetected match={secretCatch.match} workspace={secretWorkspace} onKeep={() => keepSecret(false)} onStore={() => setSecretCatch({ ...secretCatch, phase: 'name' })} />
            : <SecretNameForm key={secretCatch.match.value.length} match={secretCatch.match} workspace={secretWorkspace} onCancel={() => { setSecretCatch({ ...secretCatch, phase: 'detected' }); focusEditor() }} onStored={secretStored} />)}
        </PanelEnter>
      </PanelPresence>
      <div
        ref={(el) => {
          card.current = el
        }}
        testId="composer-drop-zone"
        onFileDragChange={(e) => setDragOver(!!e.hovered && !props.readOnly)}
        onFileDrop={(e) => {
          setDragOver(false)
          if (props.readOnly || !e.paths?.length) return
          void addFiles(e.paths)
        }}
        // Whole-window glass (`platform/glass.ts`): the frost tint over a scene blur, a hairline rim and a top highlight, no shadow.
        backdrop={frostBackdrop(g, 'composer', compact ? 14 : COMPOSER_RADIUS)}
        style={{
          position: 'relative',
          display: 'flex',
          flexDirection: 'column',
          width: '100%',
          borderRadius: compact ? 14 : COMPOSER_RADIUS,
          borderWidth: 1,
          borderColor: g.on ? g.frostRim : t.variant === 'dark' ? 'rgba(252, 252, 252, 0.052)' : 'rgba(13, 13, 13, 0.076)',
          backgroundColor: frostFill(g, 'composer', t.glass.composer),
        }}
      >
        <GlassEdge radius={compact ? 14 : COMPOSER_RADIUS} color={g.frostHighlight} />
        {completion.menu(() => (card.current && renderer.getElementBounds?.(card.current.id)?.width) || 736)}
        {/* `dragOver && border-[color:var(--color-border-focus)]` with the card's `transition-colors duration-200`. */}
        <DropHighlight over={dragOver} radius={compact ? 14 : COMPOSER_RADIUS} color={t.v('--color-border-focus')} />
        <div style={{ display: 'flex', flexDirection: 'column', paddingTop: 14, paddingBottom: 8, paddingLeft: 16, paddingRight: 16 }}>
          <Attachments
            items={attachments}
            uploading={uploading}
            onRemove={(a) => setAttachments((list) => list.filter((item) => item.id !== a.id))}
            onPreviewClose={focusEditor}
            tileNodes={tileNodes.current}
          />
          <div style={{ position: 'relative', display: 'flex' }}>
            {/* GPUIX paints its own placeholder in a fixed grey; this one is the kit's muted × 40%. */}
            {!text && (
              <div testId="composer-placeholder" paintTransform={props.placeholderEnter} style={{ position: 'absolute', top: 0, left: 0, right: 0, pointerEvents: 'none' }}>
                <Label color={muted(t, 0.4)} lineHeight={19.6} truncate>
                  {props.placeholder ?? DEFAULT_PLACEHOLDER}
                </Label>
              </div>
            )}
            <TokenTextarea
              editorRef={tokenEditor}
              onEditorError={setError}
              wash={secretCatch?.phase === 'detected' ? { start: secretCatch.match.start, end: secretCatch.match.end } : null}
              references={[...completion.tokens, ...attachmentRefs.map(r => ({ token: r.token, label: r.token.slice(1), icon: tokenIcon('attachment', r.token) }))]}
              nativeRef={(el) => {
                editor.current = el
              }}
              interceptKeys={['up', 'down', 'left', 'right', 'home', 'end', 'enter', 'tab', 'escape', 'v']}
              testId="composer"
              autoFocus={!pane.frozen && getFocusedPaneId() === pane.id}
              onFocus={() => { if (!pane.frozen) focusPane(pane.id) }}
              value={text}
              readOnly={props.readOnly || (!!props.launch && sending)}
              minRows={MIN_ROWS}
              maxRows={maxRows}
              onClick={readSelection}
              onChange={(e) => {
                readSelection()
                setFeedback(pastePending.current ? 'Pasted text' : null)
                pastePending.current = false
                if (approvalDigit.current && e.value === approvalDigit.current) setText('')
                else {
                  const next = e.value ?? ''
                  // Every way text arrives (⌘V, the menu, a drop) comes through here, so the scan reads what was inserted.
                  const hit = props.readOnly || secretCatchRef.current ? null : scanInsert(textNow.current, next)
                  if (hit && !keptSecrets.has(hit.value)) setSecretCatch({ match: hit, phase: 'detected' })
                  textNow.current = next
                  // A card waiting for a pause (`requests/typingGuard.ts`) starts its idle window over.
                  noteKeystroke()
                  setText(next)
                }
                if (error) setError(null)
              }}
              onKeyDown={(e) => {
                readSelection()
                // Not with Alt: Windows sends AltGr as Ctrl+Alt (Hungarian AltGr+V is `@`), which is text, not a paste.
                if (isPrimaryChord(e.modifiers, { shift: 'any' }) && e.key === 'v' && !props.readOnly) {
                  try {
                    const payload = clipboardAttachments(renderer)
                    if (payload.paths.length || payload.image) {
                      e.preventDefault()
                      if (payload.paths.length) void addFiles(payload.paths)
                      else void addClipboardImage(payload.image!)
                      return
                    }
                  } catch (error) { setError(`Unable to paste: ${errorText(error)}`) }
                  pastePending.current = true
                }
                // Native navigation is applied after the key notification.
                if (['left', 'right', 'up', 'down', 'home', 'end'].includes(e.key ?? '')) setTimeout(readSelection, 32)
                // Marked IME input bypasses native interception. Do not turn
                // its ordinary key notification into a completion or send.
                if (['up', 'down', 'enter', 'tab', 'escape'].includes(e.key ?? '') && (e as typeof e & { requestId?: number }).requestId == null) return
                // A caught key: ↵ goes to the name form (the message is not sent: a key must not go out by habit), ⌘↵ keeps it
                // as text and sends, Esc keeps it as text.
                const caught = secretCatchRef.current
                if (caught && e.key === 'enter' && isPrimary(e.modifiers)) { e.preventDefault(); keepSecret(true); return }
                if (caught?.phase === 'detected' && e.key === 'enter' && !e.modifiers?.shift && !e.modifiers?.alt && !e.modifiers?.ctrl) {
                  e.preventDefault()
                  if (secretWorkspace) setSecretCatch({ ...caught, phase: 'name' })
                  return
                }
                if (caught && e.key === 'escape') { e.preventDefault(); keepSecret(false); return }
                // ⌘↵ (Ctrl+Enter off macOS) steers even when a completion menu is open.
                if (e.key === 'enter' && isPrimary(e.modifiers) && !e.modifiers?.shift) {
                  e.preventDefault()
                  void submit(running ? 'steer' : promptMode)
                  return
                }
                if ((e.key === 'up' || e.key === 'down') && (e.modifiers?.cmd || e.modifiers?.alt || e.modifiers?.ctrl || e.modifiers?.shift)) return
                if (completion.onKey(e.key ?? '', !!e.modifiers?.shift, !!e.modifiers?.ctrl, !!(e.modifiers?.alt || e.modifiers?.cmd))) {
                  e.preventDefault()
                  return
                }
                if (e.key === 'down' && empty && props.onEmptyDown?.()) {
                  e.preventDefault()
                  return
                }
                if (e.key === 'enter' && !e.modifiers?.shift && !e.modifiers?.alt && !e.modifiers?.ctrl) {
                  e.preventDefault()
                  void submit()
                }
              }}
              onSubmit={() => { void submit() }}
              theme={{ caret: t.text }}
              style={{ width: '100%', fontSize: 14, lineHeight: LINE_HEIGHT, color: t.text, fontFamily: FONT_UI, opacity: connected ? 1 : 0.6 }}
            />
          </div>
          {feedback && !error && <div testId="composer-feedback"><Label color={muted(t, 0.6)} size={12}>{feedback}</Label></div>}
          {error && (
            <div style={{ display: 'flex', paddingTop: 6 }}>
              <Label color={t.red} size={12} lineHeight={16}>
                {error}
              </Label>
            </div>
          )}
        </div>
        <FooterRow style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 4, paddingLeft: 12, paddingRight: 12, paddingBottom: 12, minWidth: 0 }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: 6, flexShrink: 0, minWidth: 0 }}>
            <PickerMenu
              testId="composer-plus"
              side="top"
              align="start"
              trigger={(open) => (
                <IconButton icon="plus" label="Add" active={open} />
              )}
            >
              <PickerItem id="files" testId="add-files" onSelect={() => void addFiles()}>
                <Icon source={G.paperclip} size={16} color={t.text} style={{ opacity: 0.8 }} />
                <Label color={t.text}>Add files</Label>
              </PickerItem>
              {sendTargets(machines).length > 0 && <>
                <PickerLabel>Send clipboard to…</PickerLabel>
                {sendTargets(machines).map((target) => <PickerItem key={target.machineId} id={`clipboard-send-${target.machineId}`} testId={`clipboard-send-${target.machineId}`} onSelect={() => void sendClipboardTo(target)}>
                  <MachineBadge glyph={glyphOfMachine(target.machineId)} size={16} />
                  <Label color={t.text}>{target.name}</Label>
                </PickerItem>)}
              </>}
              {clipboardSources(machines).length > 0 && <>
                <PickerLabel>Paste from…</PickerLabel>
                {clipboardSources(machines).map((source) => <PickerItem key={source.machineId} id={`clipboard-paste-${source.machineId}`} testId={`clipboard-paste-${source.machineId}`} disabled={props.readOnly} onSelect={() => void readClipboardFrom(source).then((text) => { if (text !== null && edit(selection.start, selection.end, text)) setFeedback('Pasted text') })}>
                  <MachineBadge glyph={glyphOfMachine(source.machineId)} size={16} />
                  <Label color={t.text}>{source.name}</Label>
                </PickerItem>)}
              </>}
            </PickerMenu>
            <PermissionPicker locked={props.lockMode} mode={mode} kind={provider?.kind} status={status} onChange={props.onModeChange} />
          </div>
          <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'flex-end', gap: 4, flexGrow: 1, minWidth: 0 }}>
            {props.machinePicker}
            {provider && props.currentThreadId && <StatusGlyph provider={provider} running={running} />}
            {provider && (
              <ModelPicker
                provider={provider}
                providers={providers}
                {...(props.agentsOn ? { machineName: props.agentsOn } : {})}
                model={model}
                effort={effort}
                canPickModel={canPickModel}
                canPickProvider={canPickProvider}
                canReload={canReload}
                loading={props.catalogLoading}
                busy={changingModel}
                accounts={props.accounts}
                accountInstance={provider.instance}
                accountFollowsDefaults={props.accountFollowsDefaults}
                onAccountChange={props.onAccountChange ? (instance) => busyRun(() => props.onAccountChange!(instance)) : undefined}
                onOpenChange={(open) => {
                  // Self-heal a stale catalog without re-probing on every open.
                  if (!open || !canReload || Date.now() - lastModelRefresh.current < 60_000) return
                  lastModelRefresh.current = Date.now()
                  void props.onReloadModels(false).catch(() => {})
                }}
                onModelChange={(next, nextEffort) => busyRun(() => props.onModelChange!(next || undefined, nextEffort))}
                onEffortChange={(next) => busyRun(() => props.onModelChange!(variantSelector(current, model) ?? model ?? undefined, next))}
                onProviderChange={(next, choice) => busyRun(() => props.onProviderChange!(next, choice))}
                onReload={() => {
                  lastModelRefresh.current = Date.now()
                  void props.onReloadModels(true).catch((e) => setError(`Unable to reload models: ${errorText(e)}`))
                }}
                lead={accountLead(provider.kind, triggerAccount)}
                label={() => (
                  <ModelTriggerLabel
                    account={triggerAccount}
                    model={modelName}
                    qualifier={qualifier}
                    fast={traits.fast}
                    effort={(current?.display_name ?? model) && effortLabel ? formatEffort(effortLabel) : null}
                    traits={traits.labels}
                  />
                )}
              />
            )}
            {running && props.canSteer !== undefined && (
              <RunningControls
                sending={sending}
                steering={steering}
                canSend={canSend}
                canSteer={!!props.canSteer}
                onSubmit={() => void submit()}
                onPromptMode={setPromptMode}
              />
            )}
            <SendButton running={!!running && !props.starting} sending={sending || !!props.starting} canSend={canSend} steering={steering} disabledReason={disabledReason} onSubmit={() => void submit()} onStop={props.onStop} />
          </div>
        </FooterRow>
      </div>
    </div>
  )
}

/**
 * One button for send, sending and stop, so the glyph can cross-swap (`IconSwap`: the arrow shrinks and blurs out while the
 * spinner or the stop square grows in). A keyboard send still swaps because it reports state, not navigation, and never delays input.
 */
export function SendButton({ running, sending, canSend, steering, disabledReason, onSubmit, onStop }: { running: boolean; sending: boolean; canSend: boolean; steering: boolean; disabledReason?: string | null; onSubmit: () => void; onStop?: () => void }) {
  const c = useControls()
  const inactive = !running && !sending && !canSend
  const glyph = inactive ? c.disabled.fg : c.send.fg
  return (
    <Tip label={running ? 'Stop generation' : disabledReason ?? (steering ? 'Steer now ⌘↵' : 'Send ↵')}>
      <RoundButton testId={running ? 'stop' : 'send'} disabled={running ? false : !canSend} inactive={inactive} onClick={running ? onStop : onSubmit}>
        {/* `IconSwap` arrow ↔ spinner while sending, arrow ↔ stop square while the thread runs (`Composer.tsx:1441`); the spinner is the shared-clock loader. */}
        <SwapOnChange kind="icon" active={running ? 'stop' : sending ? 'sending' : 'send'} style={{ width: 20, height: 20 }}>
          {running ? <Icon source={G.stop} size={10} color={glyph} /> : sending ? <MatrixLoader rgb={rgbOf(glyph)} alpha={1} gap={1.5} /> : <Icon name="send" size={20} color={glyph} />}
        </SwapOnChange>
      </RoundButton>
    </Tip>
  )
}

/** `#rrggbb` or `rgb(r, g, b)` as a tuple (the loader's dot colour on the ink button). */
function rgbOf(color: string): [number, number, number] {
  const hex = color.match(/^#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})/i)
  if (hex) return [parseInt(hex[1]!, 16), parseInt(hex[2]!, 16), parseInt(hex[3]!, 16)]
  const [r = 0, g = 0, b = 0] = (color.match(/[\d.]+/g) ?? []).map(Number)
  return [r, g, b]
}

/**
 * Direction C Send (`useControls().send`): a 32 px circle (28 in a split pane) in the accent tint with the tinted glyph; an inactive
 * Send drops to the disabled fill and glyph. Stop is the same circle. It still grows to 1.05 on hover and squashes on press.
 */
function RoundButton({ children, disabled, inactive, onClick, onMouseEnter, onMouseLeave, onMouseDown, onFocus, onBlur, onKeyDown, testId }: {
  children: ReactNode
  disabled?: boolean
  /** Looks switched off (a Send with nothing to send); a sending Send is disabled but keeps its tint. */
  inactive?: boolean
  onClick?: () => void
  /** The tooltip trigger's handlers (`Tip` clones them onto its child). */
  onMouseEnter?: () => void
  onMouseLeave?: () => void
  onMouseDown?: () => void
  onFocus?: () => void
  onBlur?: () => void
  onKeyDown?: (event: { key?: string }) => void
  testId: string
}) {
  const c = useControls()
  const [hover, setHover] = useState(false)
  // `surfaceMode === "split" && "!size-7"`: 28px in a split pane.
  const size = usePaneCompact() ? 28 : 32
  return (
    <div
      testId={testId}
      role="button"
      aria-disabled={disabled}
      onClick={disabled ? undefined : onClick}
      onMouseEnter={() => { setHover(true); onMouseEnter?.() }}
      onMouseLeave={() => { setHover(false); onMouseLeave?.() }}
      onMouseDown={onMouseDown}
      onFocus={onFocus}
      onBlur={onBlur}
      onKeyDown={onKeyDown}
      paintTransform={hoverPressTransform(hover && !disabled, !disabled)}
      style={{
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        width: size,
        height: size,
        borderRadius: size / 2,
        flexShrink: 0,
        backgroundColor: inactive ? c.disabled.bg : c.send.bg,
        cursor: disabled ? 'default' : 'pointer',
        ...(disabled ? {} : { focusVisible: { outlineWidth: 2, outlineColor: c.focus, outlineOffset: 0 } }),
      }}
    >
      {children}
    </div>
  )
}

/** Queue / Steer now and the delivery menu (`Composer.tsx:1364-1402`). */
function RunningControls({
  sending,
  steering,
  canSend,
  canSteer,
  onSubmit,
  onPromptMode,
}: {
  sending: boolean
  steering: boolean
  canSend: boolean
  canSteer: boolean
  onSubmit: () => void
  onPromptMode: (mode: 'queue' | 'steer') => void
}) {
  const t = useTheme()
  // `@max-[360px]:gap-0 @max-[360px]:px-1.5`: the label goes (it stays for screen readers) and the clock, or the spinner while sending, stands in.
  const icon = !useFooterFit().model
  const text = sending ? 'Sending…' : steering ? 'Steer now' : 'Queue'
  return (
    <div style={{ display: 'flex', alignItems: 'center', gap: 2, flexShrink: 0 }}>
      <PickerChip
        bare
        testId="queue"
        chevron={false}
        disabled={!canSend}
        onClick={onSubmit}
        label={text}
        {...(icon ? (sending ? { lead: <Spinner size={12} color={t.text} /> } : { icon: 'clock' }) : {})}
      >
        {icon ? null : text}
      </PickerChip>
      {canSteer && (
        <PickerMenu
          testId="delivery"
          side="top"
          align="end"
          width={240}
          trigger={(open) => (
            <IconButton icon="chevronDown" label="Delivery" glyph={12} active={open} />
          )}
        >
          <PickerItem id="queue" onSelect={() => onPromptMode('queue')} style={{ paddingTop: 4, paddingBottom: 4 }}>
            <div style={{ display: 'flex', flexDirection: 'column', minWidth: 0 }}>
              <Label color={t.text}>{`Queue follow-up${!steering ? ' ✓' : ''}`}</Label>
              <Label color={t.textSecondary} size={12}>
                Send after the current work finishes · Enter
              </Label>
            </div>
          </PickerItem>
          <PickerItem id="steer" onSelect={() => onPromptMode('steer')} style={{ paddingTop: 4, paddingBottom: 4 }}>
            <div style={{ display: 'flex', flexDirection: 'column', minWidth: 0 }}>
              <Label color={t.text}>{`Steer now${steering ? ' ✓' : ''}`}</Label>
              <Label color={t.textSecondary} size={12}>
                {`Guide the current turn immediately · ${shortcutLabel('⌘+Enter')}`}
              </Label>
            </div>
          </PickerItem>
        </PickerMenu>
      )}
    </div>
  )
}

/** Image tiles and file cards above the editor (`Composer.tsx:1143-1171`). */
export function Attachments({ items, uploading, onRemove, onPreviewClose, tileNodes }: { items: Attachment[]; uploading: number; onRemove: (a: Attachment) => void; onPreviewClose?: () => void; tileNodes?: Map<string, PublicInstance> }) {
  const t = useTheme()
  const g = useGlass()
  const renderer = useGpuixRequired()
  // A clicked image tile morphs into the shared preview out of the tile's rect (`motion/ImageMorph.tsx`); the tile hides while its
  // image is away.
  const tiles = useRef(new Map<string, PublicInstance>())
  const [expanded, setExpanded] = useState<{ id: string; from: MorphSource | null } | null>(null)
  const tileSource = (id: string): MorphSource | null => {
    const node = tiles.current.get(id)
    const box = node ? renderer.getElementBounds?.(node.id) : null
    // The image paints inside the tile's 1px border, cropped to fill it, with the inner radius.
    return box ? { rect: { x: box.x + 1, y: box.y + 1, width: box.width - 2, height: box.height - 2 }, radius: 11, fit: 'cover' } : null
  }
  const open = expanded ? items.find(a => a.id === expanded.id) : undefined
  // The previewed attachment left (a send cleared the tray, the draft switched): forget it, or its tile would stay hidden
  // and the preview reopen from a stale rect if the same attachment comes back.
  useEffect(() => { if (expanded && !open) setExpanded(null) }, [expanded, open])
  // GPUIX delivers a click to the × and to the tile under it, so a click on a tile's × never opens the preview. Decided by
  // where the click is, not by a flag from the press: a press on the × released elsewhere has no click to clear one, and
  // the flag swallowed the next click on that tile.
  const removes = useRef(new Map<string, PublicInstance>())
  const onRemoveButton = (id: string, e: { x?: number; y?: number }) => {
    const node = removes.current.get(id), box = node ? renderer.getElementBounds?.(node.id) : null
    return !!box && e.x !== undefined && e.y !== undefined && e.x >= box.x && e.x < box.x + box.width && e.y >= box.y && e.y < box.y + box.height
  }
  const openPreview = (id: string, from: MorphSource | null) => setExpanded({ id, from })
  const away = expanded?.from && PATCHED_NATIVE && !REDUCED_MOTION ? expanded.id : null
  const elevated = t.v('--color-background-elevated-secondary')
  // Only the ✕ removal leaves with an exit; sending clears the tray at once.
  const removed = useRef(new Set<string>())
  const [entries, gone] = useLeavingList(items, (id) => removed.current.delete(id))
  const remove = (a: Attachment) => { removed.current.add(a.id); onRemove(a) }
  if (!entries.length && uploading <= 0) return null
  return (
    <div style={{ display: 'flex', flexWrap: 'wrap', alignItems: 'flex-start', gap: 6, marginLeft: -6, marginRight: -6, marginTop: -4, marginBottom: 8 }}>
      {entries.map(({ item: a, leaving }) => (
        // `t-pop` on each tile (`Composer.tsx:1147`); removing one shrinks and fades it out (the neighbours close up after).
        <Presence key={a.id} show={!leaving} exit={{ to: { opacity: 0, scale: 0.85 }, duration: 0.15 }} onGone={() => gone(a.id)} style={{ flexShrink: 0 }}>
        <Pop>
          {a.media_type.startsWith('image/') ? (
            <div
              ref={(node) => { if (node) { tiles.current.set(a.id, node); tileNodes?.set(a.id, node) } else { tiles.current.delete(a.id); tileNodes?.delete(a.id) } }}
              testId={`attachment-${a.label}`}
              role="button"
              tabIndex={0}
              aria-label={`Preview ${a.name}`}
              onClick={(e: { x?: number; y?: number }) => { if (!onRemoveButton(a.id, e)) openPreview(a.id, tileSource(a.id)) }}
              onKeyDown={(e) => { if (e.key === 'enter' || e.key === 'space') openPreview(a.id, null) }}
              style={{ position: 'relative', width: 64, height: 64, flexShrink: 0, borderRadius: 12, borderWidth: 1, borderColor: t.borderLight, backgroundColor: elevated, overflow: 'hidden', cursor: 'pointer' }}
            >
              <img src={a.path} objectFit="cover" style={{ width: 62, height: 62, borderRadius: 11, pointerEvents: 'none', opacity: away === a.id ? 0 : 1 }} />
              <div style={{ position: 'absolute', left: 4, bottom: 4, paddingLeft: 4, paddingRight: 4, borderRadius: 6, backgroundColor: 'rgba(0, 0, 0, 0.55)' }}>
                <Label color="#ffffff" size={10} lineHeight={16} weight={500}>{`@${a.label}`}</Label>
              </div>
              <RemoveButton testId={`remove-attachment-${a.label}`} nodeRef={(node) => { if (node) removes.current.set(a.id, node); else removes.current.delete(a.id) }} onClick={() => remove(a)} />
            </div>
          ) : (
            <div
              testId={`attachment-${a.label}`}
              style={{
                position: 'relative',
                display: 'flex',
                alignItems: 'center',
                gap: 10,
                height: 56,
                width: 240,
                paddingTop: 8,
                paddingBottom: 8,
                paddingLeft: 8,
                paddingRight: 32,
                borderRadius: 12,
                borderWidth: 1,
                borderColor: t.borderLight,
                backgroundColor: raisedFill(g, t.variant === 'dark' ? t.composer : '#ffffff'),
              }}
            >
              <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'center', width: 40, height: 40, flexShrink: 0, borderRadius: 8, backgroundColor: elevated }}>
                <Icon name="file" size={16} color={t.textSecondary} />
              </div>
              <div style={{ display: 'flex', flexDirection: 'column', justifyContent: 'center', gap: 2, minWidth: 0, flexGrow: 1 }}>
                <Label color={t.text} size={13} weight={500} lineHeight={16} truncate>
                  {a.name}
                </Label>
                <div style={{ display: 'flex', gap: 6 }}>
                  <Label color={t.textSecondary} size={11} weight={500} lineHeight={14}>{`@${a.label}`}</Label>
                  <Label color={t.textSecondary} size={11} weight={500} lineHeight={14}>
                    {(a.name.split('.').pop() ?? '').toUpperCase()}
                  </Label>
                </div>
              </div>
              <RemoveButton testId={`remove-attachment-${a.label}`} onClick={() => remove(a)} />
            </div>
          )}
        </Pop>
        </Presence>
      ))}
      {uploading > 0 && (
        <div style={{ display: 'flex', alignItems: 'center', gap: 6, paddingLeft: 4, paddingRight: 4, height: 24 }}>
          <MatrixLoader />
          <Label color={t.textSecondary} size={12}>
            Uploading…
          </Label>
        </div>
      )}
      {open && expanded && <ImageViewer key={open.id} testId="attachment-lightbox" src={open.path} label={open.name} from={expanded.from} source={() => tileSource(open.id)} onClose={() => { setExpanded(null); onPreviewClose?.() }} />}
    </div>
  )
}

/** A 20px ink disc with a 12px ×, top-right. */
function RemoveButton({ onClick, nodeRef, testId }: { onClick: () => void; nodeRef?: (node: PublicInstance | null) => void; testId?: string }) {
  const t = useTheme()
  return (
    <div
      ref={nodeRef}
      testId={testId}
      onClick={onClick}
      style={{
        position: 'absolute',
        top: 4,
        right: 4,
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        width: 20,
        height: 20,
        borderRadius: 10,
        cursor: 'pointer',
        backgroundColor: ink(t, 0.8),
        hover: { backgroundColor: t.text },
      }}
    >
      <Icon source={G.x} size={12} color={t.surface} />
    </div>
  )
}
