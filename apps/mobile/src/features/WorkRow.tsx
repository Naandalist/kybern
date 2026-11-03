// Work in the transcript, one compact line each: a verb and its target in mono ("Ran cargo test",
// "Edited src/…/Transcript.tsx"). Tool calls and runtime tasks share the anatomy (ux-thread S5):
// a 14 pt glyph, `footnote` verb, `code` target on one line (paths cut in the middle, commands at
// the end). Opened, a call shows what it did (the command and its output, the edit as a diff),
// with the raw input one more tap away. Running processes never list here one by one: they
// collapse into a single "3 processes running" row that opens the Agents sheet.
import { router } from "expo-router";
import { memo, useEffect, useState, type ReactNode } from "react";
import { View } from "react-native";
import Animated, { FadeIn } from "react-native-reanimated";
import { displayCommand } from "../../../../packages/kybern-client/src/displayCommand";
import { commandOf, printedText } from "../../../../packages/kybern-client/src/signIn";
import { signInRequestMessage, type LoginRecipe } from "../../../../packages/kybern-client/src/loginRecipes";
import { toolLine, toolLineCounts, unwrapShellCommand } from "../../../../packages/kybern-client/src/toolActivity";
import { runtimeTaskStatusWord } from "../../../../packages/kybern-client/src/statusWords";
import { launchCounts, launchGroupTitle, launchState, type ChildPhase } from "../../../../packages/kybern-client/src/delegations";
import { delegateTitle, orchestrationTool, parseDelegateResult, parseSendInput, parseSendResult, sendStateWord } from "../../../../packages/kybern-client/src/orchestrationTools";
import { findSubagentFor, subagentGroupTitle } from "../../../../packages/kybern-client/src/subagents";
import { purposeLabel } from "../../../../packages/kybern-client/src/delegations";
import { sendFollowUp } from "../state/followUp";
import { count } from "../state/format";
import type { JsonValue, RuntimeTask, Thread } from "../state/protocol";
import { useRemoteRecord, useRemoteRecords } from "../state/remoteDelegations";
import { errorText, getState, rpc, useApp } from "../state/runtime";
import { runtimeStatus } from "../state/status";
import type { Block } from "../state/transcript";
import { Code } from "../ui/Markdown";
import { Button, Icon, T, Tap } from "../ui/primitives";
import { ProviderMark } from "../ui/ProviderMark";
import { StatusGlyph } from "../ui/TaskGlyphs";
import { showToast } from "../ui/Toast";
import { space, useTheme } from "../ui/theme";
import { openAgents } from "./AgentsSheet";
import { CSS_EASE_OUT } from "../components/liquid/motion";

type ToolBlock = Extract<Block, { kind: "tool" }>;

/** Lines a body shows before it is cut; commands keep their tail, where the result is. */
const MAX_LINES = 80;
const tail = (text: string) => {
  const lines = text.replace(/\s+$/, "").split("\n");
  return lines.length > MAX_LINES
    ? { text: lines.slice(-MAX_LINES).join("\n"), cut: lines.length - MAX_LINES }
    : { text: lines.join("\n"), cut: 0 };
};
const head = (text: string) => {
  const lines = text.split("\n");
  return lines.length > MAX_LINES
    ? { text: lines.slice(0, MAX_LINES).join("\n"), cut: lines.length - MAX_LINES }
    : { text, cut: 0 };
};

const record = (value: unknown): Record<string, unknown> =>
  value && typeof value === "object" && !Array.isArray(value) ? (value as Record<string, unknown>) : {};
const lines = (text: unknown) => (typeof text === "string" && text ? text.replace(/\n$/, "").split("\n") : []);

/** An edit's input as a diff: replaced strings, a written file, or the patch itself. */
export function editPreview(input: JsonValue): string | null {
  const fields = record(input);
  const pairs: [unknown, unknown][] = [];
  if ("old_string" in fields || "new_string" in fields) pairs.push([fields.old_string, fields.new_string]);
  if (Array.isArray(fields.edits))
    for (const edit of fields.edits) pairs.push([record(edit).old_string, record(edit).new_string]);
  if (pairs.length)
    return pairs
      .map(([before, after]) => [...lines(before).map((l) => `-${l}`), ...lines(after).map((l) => `+${l}`)].join("\n"))
      .join("\n@@\n");
  if (typeof fields.content === "string") return lines(fields.content).map((l) => `+${l}`).join("\n");
  for (const key of ["patch", "diff", "input"])
    if (typeof fields[key] === "string" && /^[-+@]/m.test(fields[key] as string)) return fields[key] as string;
  return null;
}

/** What a row names: the cleaned command, else the activity's own target. */
export function workTarget(call: ToolBlock["call"], complete: boolean) {
  const activity = toolLine(call, complete);
  const command = activity.kind === "command" ? commandOf(call.input) || activity.detail : "";
  return {
    activity,
    target: command ? displayCommand(command, { max: 240 }).text : activity.detail,
    path: activity.kind === "read" || activity.kind === "edit" || activity.kind === "write" || activity.kind === "list",
  };
}

/** The shared line: glyph, verb, mono target, trailing meta and chevron. */
function WorkLine({
  glyph,
  verb,
  target,
  mono = true,
  path = false,
  meta,
  metaTone = "muted",
  chevron,
  label,
  expanded,
  onPress,
}: {
  glyph: ReactNode;
  verb: string;
  target: string;
  mono?: boolean;
  path?: boolean;
  meta?: string;
  metaTone?: "muted" | "negative" | "secondary";
  chevron?: "down" | "right";
  label: string;
  expanded?: boolean;
  onPress: () => void;
}) {
  const { colors } = useTheme();
  return (
    <Tap
      label={label}
      expanded={expanded}
      onPress={onPress}
      feedback="highlight"
      hitSlop={{ top: 6, bottom: 6 }}
      style={{ flexDirection: "row", alignItems: "center", gap: space.sm, minHeight: 32, borderRadius: 8, paddingHorizontal: space.xs, marginHorizontal: -space.xs }}
    >
      <View style={{ width: 14, alignItems: "center" }}>{glyph}</View>
      <T variant="footnote" tone="secondary" style={{ fontWeight: "500" }} numberOfLines={1}>
        {verb}
      </T>
      {!!target && (
        <T
          variant={mono ? "code" : "footnote"}
          tone="secondary"
          numberOfLines={1}
          ellipsizeMode={path ? "middle" : "tail"}
          style={{ flex: 1, flexShrink: 1 }}
        >
          {target}
        </T>
      )}
      {!target && <View style={{ flex: 1 }} />}
      {!!meta && (
        <T variant="footnote" tone={metaTone} style={{ fontVariant: ["tabular-nums"] }} numberOfLines={1}>
          {meta}
        </T>
      )}
      {chevron && (
        <Icon
          name={chevron === "right" ? "chevron.right" : expanded ? "chevron.up" : "chevron.down"}
          size={12}
          color={colors.muted}
        />
      )}
    </Tap>
  );
}

function CutNote({ cut, where }: { cut: number; where: "start" | "end" }) {
  if (!cut) return null;
  return (
    <T variant="footnote" tone="muted">
      {where === "start" ? `${count(cut, "earlier line")} not shown` : `${count(cut, "more line")} not shown`}
    </T>
  );
}

/** One tool call. Opened: the command and its output, or the edit as a diff; the raw input behind "Show input". */
export const ToolWorkRow = memo(function ToolWorkRow({
  block,
  threadId,
  signIn,
  expanded,
  onToggle,
}: {
  block: ToolBlock;
  threadId: string;
  signIn?: LoginRecipe;
  expanded: boolean;
  onToggle: (open: boolean) => void;
}) {
  const { colors } = useTheme();
  const { activity, target, path } = workTarget(block.call, block.complete);
  const counts = block.complete && (activity.kind === "edit" || activity.kind === "write") ? toolLineCounts(block.call) : null;
  const [input, setInput] = useState(false);
  const [fetched, setFetched] = useState<{ output: JsonValue; stream: string } | null>(null);
  const [loadError, setLoadError] = useState("");
  const [asked, setAsked] = useState(false);
  const deferred = block.complete && (block.outputOmitted || block.streamOmitted);
  useEffect(() => {
    if (!expanded || !deferred || fetched) return;
    let alive = true;
    rpc("threads.tool_output", { thread_id: threadId, tool_call_id: block.call.id, include_tool_stream: true })
      .then((result) => {
        if (alive) setFetched({ output: result.output, stream: result.stream ?? "" });
      })
      .catch((e) => {
        if (alive) setLoadError(errorText(e));
      });
    return () => {
      alive = false;
    };
  }, [expanded, deferred, fetched, threadId, block.call.id]);
  const output = fetched ? printedText(fetched.output, fetched.stream) : printedText(block.output, block.stream);
  const command = activity.kind === "command" ? unwrapShellCommand(commandOf(block.call.input)) : "";
  const diff = activity.kind === "edit" || activity.kind === "write" ? editPreview(block.call.input) : null;
  const status = block.isError ? "failed" : block.complete ? "done" : "running";
  return (
    <View>
      <WorkLine
        glyph={
          block.isError ? (
            <Icon name="xmark" size={13} color={colors.negative} />
          ) : block.complete ? (
            <Icon name="checkmark" size={13} color={colors.muted} />
          ) : (
            <StatusGlyph phase="working" size={14} animated />
          )
        }
        verb={activity.verb}
        target={target}
        mono={activity.mono}
        path={path}
        meta={counts ? `+${counts.additions} −${counts.deletions}` : block.isError ? "Failed" : undefined}
        metaTone={block.isError ? "negative" : "muted"}
        chevron="down"
        label={`${activity.verb} ${target}, ${status}. ${expanded ? "Hide" : "Show"} details`}
        expanded={expanded}
        onPress={() => onToggle(!expanded)}
      />
      {signIn && (
        <View style={{ paddingStart: 22, paddingVertical: space.xs, alignItems: "flex-start" }}>
          <Button
            size="medium"
            icon="key"
            disabled={asked}
            onPress={() => {
              setAsked(true);
              void sendFollowUp(threadId, signInRequestMessage(signIn))
                .then(() => showToast(`Asked to sign in to ${signIn.service}`))
                .catch((e) => {
                  setAsked(false);
                  showToast(errorText(e));
                });
            }}
          >
            {`Sign in to ${signIn.service}`}
          </Button>
        </View>
      )}
      {expanded && (
        <Animated.View entering={FadeIn.duration(160)} style={{ paddingStart: 22, gap: space.xs }}>
          {!!command && <Code text={command} language="Command" />}
          {diff ? (
            (() => {
              const shown = head(diff);
              return (
                <>
                  <Code text={shown.text} diff language={counts ? `Changes +${counts.additions} −${counts.deletions}` : "Changes"} />
                  <CutNote cut={shown.cut} where="end" />
                </>
              );
            })()
          ) : output.trim() ? (
            (() => {
              const shown = tail(output);
              return (
                <>
                  <CutNote cut={shown.cut} where="start" />
                  <Code text={shown.text} language={block.isError ? "Error" : "Output"} />
                </>
              );
            })()
          ) : (
            <T variant="footnote" tone="muted" style={{ paddingVertical: space.xs }}>
              {loadError || (deferred && !fetched ? "Loading output…" : block.complete ? "No output" : "Waiting for output…")}
            </T>
          )}
          <Tap
            label={input ? "Hide input" : "Show input"}
            onPress={() => setInput(!input)}
            expanded={input}
            style={{ alignSelf: "flex-start", justifyContent: "center", minHeight: 36 }}
          >
            <T variant="footnote" tone="accent">
              {input ? "Hide input" : "Show input"}
            </T>
          </Tap>
          {input && <Code text={JSON.stringify(block.call.input, null, 2)} language={block.call.name} />}
        </Animated.View>
      )}
    </View>
  );
});

/** An agent task opens its own thread when the app has it; anything else opens its activity. */
function openTask(task: RuntimeTask) {
  const child = task.kind === "agent" ? findSubagentFor(getState().threads, { taskId: task.id, toolCallId: task.tool_call_id }) : undefined;
  if (child) router.push({ pathname: "/thread/[id]", params: { id: child.id } });
  else router.push({ pathname: "/tasks", params: { threadId: task.thread_id, taskId: task.id } });
}

/** "Delegated Fixer · Working": the child's own state once its thread is known. Opens the child. */
export const DelegationRow = memo(function DelegationRow({ block }: { block: ToolBlock }) {
  const app = useApp();
  const result = parseDelegateResult(block.output);
  const child = result ? app.threads.find((thread) => thread.id === result.threadId) : undefined;
  const title = delegateTitle(block.call.input, result);
  // A child on another machine: the computer's own record says when it ended, whatever this phone's copy says.
  const record = useRemoteRecord(result?.taskId);
  const state = launchState(child, block, record);
  return (
    <WorkLine
      glyph={<StatusGlyph phase={state.phase} size={14} animated={state.phase === "working"} />}
      verb="Delegated"
      target={title}
      mono={false}
      meta={state.word}
      metaTone={state.phase === "failed" ? "negative" : "muted"}
      chevron={result ? "right" : undefined}
      label={`Delegated ${title}, ${state.word}${result ? ". Open the agent's thread" : ""}`}
      onPress={() => result && router.push({ pathname: "/thread/[id]", params: { id: result.threadId } })}
    />
  );
});

/** Title and harness of a thread outside the loaded list (another space, a
 * subagent, an archived thread), fetched once per id and kept for the session. */
type ThreadLabel = { title: string; provider: Thread["provider"]["kind"] };
const threadLabels = new Map<string, ThreadLabel | null>();
function useThreadLabel(id: string | undefined): ThreadLabel | undefined {
  const app = useApp();
  const known = id ? app.threads.find((thread) => thread.id === id) : undefined;
  const [fetched, setFetched] = useState(() => (id ? threadLabels.get(id) ?? undefined : undefined));
  useEffect(() => {
    if (!id || known || threadLabels.has(id)) return;
    threadLabels.set(id, null);
    let live = true;
    rpc("threads.get", { thread_id: id, transcript_limit: 1, defer_tool_stream: true })
      .then(({ thread }) => {
        const label = { title: thread.title, provider: thread.provider.kind };
        threadLabels.set(id, label);
        if (live) setFetched(label);
      })
      .catch(() => threadLabels.delete(id));
    return () => {
      live = false;
    };
  }, [id, known]);
  return known ? { title: known.title, provider: known.provider.kind } : fetched;
}

/** "Sent to ✳ Reviewer · Question · Answered": a message to another thread, the
 * recipient's harness mark and what became of it. */
export const SendRow = memo(function SendRow({ block }: { block: ToolBlock }) {
  const { colors } = useTheme();
  const input = parseSendInput(block.call.input);
  const target = useThreadLabel(input.threadId);
  const word = sendStateWord(parseSendResult(block.output), block.complete, block.isError);
  const title = target?.title || "another thread";
  const verb = block.complete ? "Sent to" : "Sending to";
  return (
    <WorkLine
      glyph={
        target && !block.isError ? (
          <ProviderMark kind={target.provider} size={14} />
        ) : (
          <Icon name="paperplane" size={13} color={block.isError ? colors.negative : colors.muted} />
        )
      }
      verb={verb}
      target={`${title} · ${purposeLabel(input.purpose)}`}
      mono={false}
      meta={word}
      metaTone={block.isError ? "negative" : "muted"}
      chevron={input.threadId ? "right" : undefined}
      label={`${verb} ${title}, ${purposeLabel(input.purpose)}, ${word}${input.threadId ? ". Open that thread" : ""}`}
      onPress={() => input.threadId && router.push({ pathname: "/thread/[id]", params: { id: input.threadId } })}
    />
  );
});

/** Delegation tools that read as their own rows; null for every other call. */
export function orchestrationRowFor(block: ToolBlock) {
  const tool = orchestrationTool(block.call.name);
  if (tool === "agent_delegate") return <DelegationRow block={block} />;
  if (tool === "thread_send") return <SendRow block={block} />;
  return null;
}

/** "3 subagents · 2 working": every agent a turn started, as one disclosure. */
export const AgentsGroupRow = memo(function AgentsGroupRow({
  launches,
  expanded,
  onPress,
}: {
  launches: Block[];
  expanded: boolean;
  onPress: () => void;
}) {
  const app = useApp();
  const records = useRemoteRecords();
  const phases: ChildPhase[] = launches.map((block) => {
    if (block.kind === "runtime_task") {
      const phase = runtimeStatus(block.task.status).phase;
      return phase === "queued" ? "working" : (phase as ChildPhase);
    }
    if (block.kind === "tool") {
      const result = parseDelegateResult(block.output);
      return launchState(result ? app.threads.find((thread) => thread.id === result.threadId) : undefined, block, result ? records.get(result.taskId) : undefined).phase;
    }
    return "done";
  });
  const counts = launchCounts(phases);
  const delegated = launches.every((block) => block.kind === "tool");
  const subagents = launches.every((block) => block.kind === "runtime_task");
  const title = delegated ? launchGroupTitle(launches.length) : subagents ? subagentGroupTitle(launches.length) : count(launches.length, "agent");
  const parts = [
    counts.working ? `${counts.working} working` : null,
    counts.failed ? `${counts.failed} failed` : null,
    counts.done && (counts.working || counts.failed) ? `${counts.done} done` : null,
  ].filter(Boolean);
  return (
    <WorkSummaryRow label={parts.length ? `${title} · ${parts.join(" · ")}` : title} failed={null} expanded={expanded} onPress={onPress} />
  );
});

/** A runtime task as one work line: "Ran npm test", its status when it did not simply finish. */
export const TaskWorkRow = memo(function TaskWorkRow({ task }: { task: RuntimeTask }) {
  const info = runtimeStatus(task.status);
  const command = displayCommand(task.title, { max: 240 }).text;
  const agent = task.kind === "agent";
  const verb = agent ? "Agent" : task.kind === "monitor" ? (info.active ? "Watching" : "Watched") : info.active ? "Running" : "Ran";
  const word = runtimeTaskStatusWord(task.status, task.backgrounded);
  return (
    <WorkLine
      glyph={<StatusGlyph phase={info.phase} size={14} animated={info.active} />}
      verb={verb}
      target={command}
      mono={!agent}
      meta={info.phase === "done" ? undefined : word}
      metaTone={info.phase === "failed" ? "negative" : "muted"}
      chevron="right"
      label={`${verb} ${command}, ${word}. Open activity`}
      onPress={() => openTask(task)}
    />
  );
});

/** Every process still running in a turn, as one row that opens the Agents sheet. */
export const ProcessesRow = memo(function ProcessesRow({
  threadId,
  tasks,
}: {
  threadId: string;
  tasks: RuntimeTask[];
}) {
  const only = tasks.length === 1 ? tasks[0]! : null;
  const title = only
    ? displayCommand(only.title, { max: 160 }).text
    : count(tasks.length, "process", "processes") + " running";
  const label = only ? `${only.kind === "monitor" ? "Watching" : "Running"} ${title}` : title;
  return (
    <WorkLine
      glyph={<StatusGlyph phase="working" size={14} animated />}
      verb={only ? (only.kind === "monitor" ? "Watching" : "Running") : title}
      target={only ? title : ""}
      meta={only && only.backgrounded ? "In background" : undefined}
      chevron="right"
      label={`${label}. Open processes`}
      onPress={() => openAgents(threadId, "processes")}
    />
  );
});

/** The disclosure over a turn's folded work: "12 steps · 3m 43s", or a live run's summary. */
export const WorkSummaryRow = memo(function WorkSummaryRow({
  label,
  failed,
  expanded,
  onPress,
}: {
  label: string;
  failed: string | null;
  expanded: boolean;
  onPress: () => void;
}) {
  const { colors } = useTheme();
  return (
    <Tap
      label={`${expanded ? "Hide" : "Show"} steps. ${label}${failed ? `, ${failed}` : ""}`}
      expanded={expanded}
      onPress={onPress}
      feedback="highlight"
      style={{ flexDirection: "row", alignItems: "center", gap: space.sm, minHeight: 36, borderRadius: 8, paddingHorizontal: space.xs, marginHorizontal: -space.xs }}
    >
      <T variant="footnote" tone="secondary" style={{ fontWeight: "500", flexShrink: 1 }} numberOfLines={1}>
        {label}
      </T>
      {!!failed && (
        <T variant="footnote" tone="negative" numberOfLines={1}>
          {failed}
        </T>
      )}
      <Animated.View
        style={{
          transform: [{ rotate: expanded ? "90deg" : "0deg" }],
          transitionProperty: "transform",
          transitionDuration: 180,
          transitionTimingFunction: CSS_EASE_OUT,
        }}
      >
        <Icon name="chevron.right" size={12} color={colors.muted} />
      </Animated.View>
      <View style={{ flex: 1 }} />
    </Tap>
  );
});
