import { threadHasActivity } from "../src/state/runtime";
import { router, useFocusEffect } from "expo-router";
import { useCallback, useEffect, useMemo, useState } from "react";
import { ScrollView, View, useWindowDimensions } from "react-native";
import { KeyboardStickyView } from "react-native-keyboard-controller";
import Animated, { FadeIn, ReduceMotion } from "react-native-reanimated";
import { useSafeAreaInsets } from "react-native-safe-area-context";
import { Composer } from "../src/features/Composer";
import { MachineTitle } from "../src/features/MachineFilter";
import { MachineChip } from "../src/features/MachineChip";
import { MachineGlyph } from "../src/features/MachineGlyph";
import { SpaceSwitcher } from "../src/features/Spaces";
import { setDraft, useDraft } from "../src/state/draft";
import { plural } from "../src/state/format";
import { isFreeChatProject, type Thread, type UserMessage } from "../src/state/protocol";
import { ensureThread, refresh, rpc, startNewChat, supportsOptimisticCreate, useApp, useMachineView } from "../src/state/runtime";
import { homeReach } from "../src/state/homeReach";
import { useLists } from "../src/state/useLists";
import { useMachineTarget, pickMachine } from "../src/state/machineTarget";
import { useProviders } from "../src/state/machineProviders";
import { queueNewChat } from "../src/state/outbox";
import { newThreadId } from "../src/state/ids";
import { groupCreateFields, supportsWorktree } from "../src/state/groups";
import { loadSpacesThroughNetwork, useInSpace, useSpaceProjects } from "../src/state/spaces";
import { relativeTime } from "../src/state/threadList";
import { isGroupProject } from "../../../packages/kybern-client/src/groups";
import { optimisticThread } from "../../../packages/kybern-client/src/newChat";
import { isSubagentThread } from "../../../packages/kybern-client/src/subagents";
import { useLayout } from "../src/state/layout";
import { ChromeBar, ChromeProvider, useChromeInsets } from "../src/ui/ChromeBar";
import { ListRow, SectionHeader } from "../src/ui/List";
import { Button, Icon, IconButton, T, Tap, type IconName } from "../src/ui/primitives";
import { CONTENT_MAX_WIDTH, gutter, radius, space, useTheme } from "../src/ui/theme";
import { useSendTransition } from "../src/components/liquid/SendTransition";
import { DRAFT_SEND_THREAD, outgoingBlock } from "../src/state/sendTransition";
import { UserMessageBubble } from "../src/features/UserMessageBubble";

/** Each header side fits two 44 pt buttons, so the title stays centred. */
const HEADER_SIDE = 96;

/** Rows in Continue. */
const PICKUP_ROWS = 3;

const SUGGESTIONS: readonly { icon: IconName; title: string; prompt: string }[] = [
  { icon: "hammer", title: "Build something new", prompt: "Help me build " },
  {
    icon: "viewfinder",
    title: "Explore this project",
    prompt: "Explore this project and explain how it is organized.",
  },
  {
    icon: "arrow.triangle.branch",
    title: "Review recent changes",
    prompt: "Review the recent changes in this project. Look for bugs and improvements.",
  },
];

/** The project's most recent top-level threads (no subagents, delegated children or archived). */
function continueThreads(threads: readonly Thread[], projectId: string | undefined) {
  if (!projectId) return [];
  return threads
    .filter(
      (t) =>
        t.project_id === projectId &&
        !t.parent_thread_id &&
        !isSubagentThread(t) &&
        !t.delegation &&
        t.status !== "archived",
    )
    .sort((a, b) => b.updated_at.localeCompare(a.updated_at))
    .slice(0, PICKUP_ROWS);
}

/** A quiet chip in the footer: an icon, a value and a chevron. */
/** A chip of the row above the composer. It shrinks with the row and truncates its value; `iconOnly` keeps just the icon. */
function FooterChip({
  icon,
  label,
  value,
  onPress,
  chevron = true,
  iconOnly = false,
}: {
  icon: IconName;
  label: string;
  value: string;
  onPress: () => void;
  chevron?: boolean;
  iconOnly?: boolean;
}) {
  const { colors } = useTheme();
  return (
    <Tap
      label={label}
      feedback="highlight"
      onPress={onPress}
      style={{
        flexDirection: "row",
        alignItems: "center",
        gap: 6,
        minHeight: 36,
        paddingHorizontal: space.sm,
        borderRadius: radius.full,
        flexShrink: iconOnly ? 0 : 1,
        // An icon-only chip keeps a full touch target; a labelled one may shrink to its icon and an ellipsis.
        minWidth: iconOnly ? 44 : 0,
      }}
    >
      <Icon name={icon} size={15} color={colors.secondary} />
      {!iconOnly && (
        <T variant="footnote" tone="secondary" numberOfLines={1} style={{ flexShrink: 1 }}>
          {value}
        </T>
      )}
      {chevron && !iconOnly && <Icon name="chevron.down" size={9} color={colors.muted} />}
    </Tap>
  );
}

function HomeScreen() {
  const app = useApp();
  const lists = useLists();
  const draft = useDraft();
  const { colors } = useTheme();
  const { width, fontScale } = useWindowDimensions();
  const { regular, sidebarOpen, toggleSidebar } = useLayout();
  const insets = useSafeAreaInsets();
  const chrome = useChromeInsets();
  const sendMotion = useSendTransition();
  const [focused, setFocused] = useState(true);
  useFocusEffect(
    useCallback(() => {
      setFocused(true);
      return () => setFocused(false);
    }, []),
  );
  const outgoing =
    sendMotion.outgoing?.sourceThreadId === DRAFT_SEND_THREAD
      ? sendMotion.outgoing
      : null;
  useEffect(() => {
    if (focused && outgoing?.receipt && sendMotion.flight?.id !== outgoing.id)
      // On a wide layout the home screen is the detail pane, so swap it in place
      // rather than pushing another entry onto the detail stack.
      (regular ? router.navigate : router.push)({
        pathname: "/thread/[id]",
        params: { id: outgoing.receipt.threadId, created: "1" },
      });
  }, [
    focused,
    regular,
    outgoing?.receipt?.threadId,
    outgoing?.id,
    sendMotion.flight?.id,
  ]);
  const [prompt, setPrompt] = useState("");
  // The computer the phone is at answers (`connected`), or another machine of the network does (`reachable`): Home
  // works either way, and only shows the way to Connections when nothing answers.
  const view = useMachineView();
  const { connected, reachable } = homeReach(app.status, view.machines);
  useEffect(() => {
    if (!connected && reachable) loadSpacesThroughNetwork();
  }, [connected, reachable, view.version]);
  // Home works in the active space: a draft project from another space gives way to this one's first.
  const spaceProjects = useSpaceProjects();
  const project =
    spaceProjects.find((p) => p.id === draft.projectId) ??
    spaceProjects.find((p) => !isFreeChatProject(p.id)) ??
    spaceProjects[0];
  const workspaceLabel = draft.worktree ? "Worktree" : "This checkout";
  // Where the thread runs (`state/machineTarget.ts`): the last machine used for the project, else the one the phone is at.
  const target = useMachineTarget(project);
  // Another machine, or the computer the phone is at while it is away: a send there goes through the network (and waits in the outbox when that machine cannot take it now).
  const chosenMachine = target.remote || !target.reachable ? target.chosen : undefined;
  // The agents follow the machine: another machine's come from that machine.
  const providers = useProviders(chosenMachine ? { machineId: chosenMachine.machineId, reachable: target.reachable } : undefined);
  const recent = useMemo(() => continueThreads(lists.threads, project?.id), [lists.threads, project?.id]);
  const active = lists.threads.filter((t) =>
    threadHasActivity(t, lists.activity),
  ).length;
  useEffect(() => {
    if (project && project.id !== draft.projectId)
      setDraft({ projectId: project.id });
    if (
      providers.length &&
      !providers.some((p) => p.kind === draft.provider && p.available)
    ) {
      const first = providers.find((p) => p.available);
      if (first)
        setDraft({
          provider: first.kind,
          instance: first.instances[0] ?? "default",
          model: "",
          effort: "",
          permission: first.supported_permission_modes.includes(draft.permission)
            ? draft.permission : first.supported_permission_modes[0] ?? "supervised",
        });
    }
    const selected = providers.find((p) => p.kind === draft.provider && p.available);
    if (selected && !selected.supported_permission_modes.includes(draft.permission)) {
      setDraft({ permission: selected.supported_permission_modes[0] ?? "supervised" });
    }
  }, [project?.id, providers, draft.projectId, draft.provider, draft.permission]);
  async function send(message: UserMessage) {
    if (!project) throw new Error("Add a project before starting a thread.");
    const provider = { kind: draft.provider, instance: draft.instance };
    // The checkout on the chosen machine; the machine that owns the project instance gets the request.
    const chosen = chosenMachine?.project ?? project;
    const worktree = draft.worktree && supportsWorktree(chosen);
    const params = {
      project_id: chosen.id,
      provider,
      permission_mode: draft.permission,
      use_worktree: worktree,
      ...(draft.model ? { model: draft.model } : {}),
      ...(draft.effort ? { effort: draft.effort } : {}),
      // A group names a base branch per repository instead.
      ...(draft.baseBranch && !isGroupProject(chosen) ? { base_branch: draft.baseBranch } : {}),
      ...groupCreateFields(chosen, draft, worktree),
    };
    if (target.show && target.chosen) pickMachine(target.key, target.chosen.machineId);
    if (chosenMachine && !target.reachable) {
      // The machine is away: the thread is ours, shown at once, and its create waits in the outbox ("Waiting for satyamas").
      const threadId = newThreadId();
      const messageId = newThreadId();
      startNewChat(
        {
          threadId,
          messageId,
          message,
          worktree,
          machineId: chosenMachine.machineId,
          machineName: chosenMachine.name,
          thread: optimisticThread({ id: threadId, projectId: project.id, provider, model: draft.model || null, effort: draft.effort || null, permissionMode: draft.permission, message }),
        },
        (first, id) => queueNewChat(chosenMachine.machineId, { ...params, message: first, thread_id: threadId, message_id: id }),
      );
      return { threadId, messageId };
    }
    if (chosenMachine || !supportsOptimisticCreate()) {
      // Another machine (or an older daemon) names the thread itself: wait for it, read that machine's list, then open the thread.
      const thread = await rpc("threads.create", { ...params, message });
      await refresh(chosenMachine?.machineId).catch(() => {});
      await ensureThread(thread.id).catch(() => {});
      return { threadId: thread.id };
    }
    // Optimistic: the thread id is ours, so the receipt is immediate and the thread opens with the
    // message in flight while the daemon makes it. A failure shows on the thread with Retry.
    const threadId = newThreadId();
    const messageId = newThreadId();
    startNewChat(
      {
        threadId,
        messageId,
        message,
        worktree,
        thread: optimisticThread({
          id: threadId,
          projectId: project.id,
          provider,
          model: draft.model || null,
          effort: draft.effort || null,
          permissionMode: draft.permission,
          message,
        }),
      },
      (first, id) => rpc("threads.create", { ...params, message: first, thread_id: threadId, message_id: id }),
    );
    return { threadId, messageId };
  }
  const pickup = reachable && !outgoing && recent.length > 0;
  const open = (thread: Thread) =>
    router.push({ pathname: "/thread/[id]", params: { id: thread.id } });
  return (
    <View style={{ flex: 1, backgroundColor: colors.background }}>
      {regular ? (
        // The sidebar already carries the brand, threads, and settings, so the
        // split-view home is a clean canvas — just a way back to the rail when
        // it is collapsed.
        <View
          style={{
            paddingTop: insets.top + space.sm,
            paddingHorizontal: space.md,
            paddingBottom: space.xs,
            minHeight: 40,
            flexDirection: "row",
          }}
        >
          {!sidebarOpen && (
            <IconButton
              name="sidebar.left"
              label="Show sidebar"
              onPress={toggleSidebar}
            />
          )}
        </View>
      ) : (
        // 44 pt targets on an 8 pt grid: Threads leads, the computer (a switcher)
        // centres, Tasks and Notes trail. Equal side columns keep the title centred.
        // Settings lives on the Threads screen.
        <View
          style={{
            paddingTop: insets.top + space.xs,
            paddingHorizontal: space.sm,
            flexDirection: "row",
            alignItems: "center",
          }}
        >
          <View style={{ flex: 1, minWidth: HEADER_SIDE, alignItems: "flex-start" }}>
            <IconButton
              name="list.bullet"
              label="Threads"
              onPress={() => router.push("/library")}
            />
          </View>
          <View style={{ flexShrink: 1, alignItems: "center" }}>
            <MachineTitle />
          </View>
          <View style={{ flex: 1, minWidth: HEADER_SIDE, flexDirection: "row", justifyContent: "flex-end", gap: space.sm }}>
            <IconButton
              name="checklist"
              label="Tasks"
              onPress={() => router.push("/todo")}
            />
            <IconButton
              name="doc.text"
              label="Notes"
              onPress={() => router.push("/notes")}
            />
          </View>
        </View>
      )}
      <ScrollView
        keyboardShouldPersistTaps="handled"
        keyboardDismissMode="interactive"
        contentContainerStyle={{
          flexGrow: 1,
          paddingTop: space.xl,
          paddingBottom: chrome.bottom + space.lg,
          width: "100%",
          maxWidth: CONTENT_MAX_WIDTH,
          alignSelf: "center",
        }}
      >
        {outgoing ? (
          <View style={{ paddingHorizontal: gutter(width) }}>
            <UserMessageBubble
              block={outgoingBlock(outgoing)}
              threadId={outgoing.threadId}
            />
          </View>
        ) : (
          <Animated.View
            entering={FadeIn.duration(280).reduceMotion(ReduceMotion.System)}
            style={{ flexGrow: 1, justifyContent: "center", paddingHorizontal: gutter(width), paddingBottom: space.xl }}
          >
            {/* The space Home works in, always shown so New space and Manage spaces stay one tap away; switching refills Continue and the project chip. */}
            {reachable && (
              <View style={{ alignSelf: "center", marginBottom: space.md }}>
                <SpaceSwitcher compact always />
              </View>
            )}
            <T variant="largeTitle" accessibilityRole="header" style={{ textAlign: "center" }}>
              What are we building?
            </T>
            {!reachable && (
              <T variant="subhead" tone="secondary" style={{ marginTop: space.sm, maxWidth: 320, alignSelf: "center", textAlign: "center" }}>
                Work with your coding agents from anywhere.
              </T>
            )}
            {!reachable && (
              <View style={{ marginTop: space.xl, alignSelf: "center" }}>
                <Button
                  onPress={() =>
                    app.activeId
                      ? router.push("/settings")
                      : router.push("/connect")
                  }
                  icon="arrow.right"
                >
                  {app.activeId ? "Open connections" : "Connect your computer"}
                </Button>
              </View>
            )}
          </Animated.View>
        )}
        {pickup && (
          <View>
            {recent.length > 0 && (
              <>
                <SectionHeader
                  title="Continue"
                  action={{ title: "See all", label: "See all threads", onPress: () => router.push("/library") }}
                  inset
                  first
                />
                {recent.map((thread, index) => {
                  const working = threadHasActivity(thread, lists.activity);
                  const home = lists.machineOfThread(thread.id);
                  return (
                    <ListRow
                      key={thread.id}
                      dimmed={home?.offline}
                      trailing={home?.showGlyph ? <MachineGlyph glyph={home.glyph} /> : undefined}
                      title={thread.title || "Untitled thread"}
                      meta={
                        thread.status === "awaiting-approval"
                          ? "Needs you"
                          : working
                            ? "Working"
                            : relativeTime(thread.updated_at)
                      }
                      separator={index < recent.length - 1}
                      onPress={() => open(thread)}
                    />
                  );
                })}
              </>
            )}
          </View>
        )}
        {reachable && !outgoing && !pickup && (
          <View>
            {SUGGESTIONS.map((item) => (
              <ListRow
                key={item.title}
                title={item.title}
                leading={item.icon}
                onPress={() => setPrompt(item.prompt)}
              />
            ))}
          </View>
        )}
      </ScrollView>
      <KeyboardStickyView
        offset={{ closed: 0, opened: insets.bottom }}
        style={{ position: "absolute", start: 0, end: 0, bottom: 0 }}
      >
        <ChromeBar edge="bottom" style={{ position: "relative" }}>
          <View
            style={{
              width: "100%",
              maxWidth: CONTENT_MAX_WIDTH,
              alignSelf: "center",
              paddingTop: space.xs,
              paddingBottom: space.sm,
              gap: space.xs,
            }}
          >
            <View
              style={{
                flexDirection: "row",
                alignItems: "center",
                justifyContent: "space-between",
                gap: space.sm,
                paddingHorizontal: space.sm,
              }}
            >
              <View style={{ flexDirection: "row", alignItems: "center", gap: space.xs, flexShrink: 1, minWidth: 0 }}>
                <FooterChip
                  icon="folder"
                  label={project ? `Change project, ${project.name}` : "Add a project"}
                  value={project?.name ?? (reachable ? "Choose a project" : "No computer connected")}
                  chevron={!!project}
                  onPress={() => router.push(reachable ? "/project-picker" : "/connect")}
                />
                {reachable ? <MachineChip target={target} /> : null}
                {reachable && supportsWorktree(project) && (
                  <FooterChip
                    icon="arrow.triangle.branch"
                    label={`Thread setup, ${workspaceLabel}`}
                    value={workspaceLabel}
                    // Beside the machine chip on a phone (or with large text) the row has no room for three labels:
                    // the setup chip keeps its icon, and the project and machine names truncate.
                    iconOnly={target.show && width / fontScale < 430}
                    onPress={() => router.push("/configure")}
                  />
                )}
              </View>
              {(active > 0 || app.approvals.length > 0) && (
                <Tap
                  label="View agent activity"
                  feedback="highlight"
                  onPress={() => router.push("/activity")}
                  style={{ flexDirection: "row", alignItems: "center", minHeight: 36, paddingHorizontal: space.sm, borderRadius: radius.full, flexShrink: 0 }}
                >
                  <T variant="footnote" tone="accent" style={{ fontWeight: "600" }}>
                    {app.approvals.length
                      ? plural(app.approvals.length, { one: "{n} needs you", other: "{n} need you" })
                      : `${active} working`}
                  </T>
                </Tap>
              )}
            </View>
            <Composer
              onSend={send}
              disabled={!reachable || !project}
              prompt={prompt}
              onPromptConsumed={() => setPrompt("")}
            />
          </View>
        </ChromeBar>
      </KeyboardStickyView>
    </View>
  );
}

export default function Home() {
  return (
    <ChromeProvider>
      <HomeScreen />
    </ChromeProvider>
  );
}
