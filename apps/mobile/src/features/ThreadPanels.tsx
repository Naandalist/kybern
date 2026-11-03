// Panels that sit above the composer for the whole thread, not one turn: the account hit its usage
// limit (switch to another account and continue), and a permission change waiting for the turn
// to end (stop and apply now). Both read `threads.target.get`, refreshed when the thread's status changes.
import { randomUUID } from "expo-crypto";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { StyleSheet, View } from "react-native";
import type { AccountSummary, Thread, ThreadTargetState } from "../state/protocol";
import { hasSignIn } from "../../../../packages/kybern-client/src/machineAgents";
import { errorText, loadThread, rpc } from "../state/runtime";
import { MenuButton } from "../ui/ActionMenu";
import { Button, ErrorBanner, Icon, T } from "../ui/primitives";
import { radius, space, useTheme } from "../ui/theme";

const PERMISSION_NAMES: Record<string, string> = {
  supervised: "Ask before acting",
  "accept-edits": "Allow edits",
  auto: "Automatic",
  "full-access": "Full access",
};

function Panel({ children }: { children: ReactNode }) {
  const { colors } = useTheme();
  return (
    <View
      style={{
        padding: space.md,
        gap: space.sm,
        borderRadius: radius.md,
        borderCurve: "continuous",
        backgroundColor: colors.card,
        borderWidth: StyleSheet.hairlineWidth,
        borderColor: colors.line,
      }}
    >
      {children}
    </View>
  );
}

export function ThreadPanels({ thread }: { thread: Thread | null | undefined }) {
  const [target, setTarget] = useState<ThreadTargetState | null>(null);
  const id = thread?.id;
  useEffect(() => {
    if (!id) return;
    let alive = true;
    rpc("threads.target.get", { thread_id: id })
      .then((result) => alive && setTarget(result))
      .catch(() => alive && setTarget(null));
    return () => {
      alive = false;
    };
  }, [id, thread?.status, thread?.permission_mode]);
  if (!thread || !target) return null;
  return (
    <>
      {!!target.pending_permission_mode && <PermissionChange thread={thread} target={target} onApplied={setTarget} />}
      {target.quota_limited && <LimitRecovery thread={thread} />}
    </>
  );
}

function PermissionChange({ thread, target, onApplied }: { thread: Thread; target: ThreadTargetState; onApplied: (next: ThreadTargetState | null) => void }) {
  const { colors } = useTheme();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const changed = target.target.provider.kind !== thread.provider.kind || target.target.provider.instance !== thread.provider.instance;
  const mode = PERMISSION_NAMES[target.pending_permission_mode!] ?? target.pending_permission_mode;
  return (
    <Panel>
      <View style={{ flexDirection: "row", gap: space.sm, alignItems: "center" }}>
        <Icon name="lock.shield" size={16} color={colors.secondary} />
        <T variant="footnote" tone="secondary" style={{ flex: 1 }}>
          {changed ? `The selected agent uses ${mode} with your next message.` : `Permissions change to ${mode} after this turn.`}
        </T>
        {!changed && (
          <Button
            size="medium"
            busy={busy}
            onPress={() => {
              setBusy(true);
              setError("");
              rpc("threads.permissions.apply", { thread_id: thread.id })
                .then(() => rpc("threads.target.get", { thread_id: thread.id }))
                .then(onApplied)
                .then(() => loadThread(thread.id))
                .catch((e) => setError(errorText(e)))
                .finally(() => setBusy(false));
            }}
          >
            Stop and apply now
          </Button>
        )}
      </View>
      <ErrorBanner error={error} />
    </Panel>
  );
}

function LimitRecovery({ thread }: { thread: Thread }) {
  const { colors } = useTheme();
  const [accounts, setAccounts] = useState<AccountSummary[] | null>(null);
  const [selected, setSelected] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const attempt = useRef<string | null>(null);
  useEffect(() => {
    let alive = true;
    rpc("providers.accounts.list", { kind: thread.provider.kind })
      .then((result) => alive && setAccounts(result.accounts))
      .catch(() => alive && setAccounts([]));
    return () => {
      alive = false;
    };
  }, [thread.provider.kind]);
  const options = (accounts ?? []).filter(
    (account) =>
      account.provider.kind === thread.provider.kind &&
      account.provider.instance !== thread.provider.instance &&
      (!hasSignIn(account.provider.kind) || (account.status !== "needs_sign_in" && account.status !== "signed_out")),
  );
  const choice = options.find((account) => account.provider.instance === selected) ?? options[0];
  return (
    <Panel>
      <View style={{ flexDirection: "row", gap: space.sm }}>
        <Icon name="exclamationmark.triangle" size={16} color={colors.warning} />
        <View style={{ flex: 1, gap: space["2xs"] }}>
          <T variant="subhead" style={{ fontWeight: "500" }}>
            Account usage limit reached
          </T>
          <T variant="footnote" tone="secondary">
            {choice
              ? "Choose another account to continue the interrupted task in this conversation."
              : accounts === null
                ? "Looking for another account…"
                : "Add another account on your computer to continue, or wait for the limit to reset."}
          </T>
        </View>
      </View>
      {choice && (
        <View style={{ flexDirection: "row", alignItems: "center", gap: space.sm }}>
          <MenuButton
            label="Account to continue with"
            items={options.map((account) => ({
              title: account.name || account.provider.instance,
              subtitle: account.identity?.email,
              checked: account.provider.instance === choice.provider.instance,
              onPress: () => {
                setSelected(account.provider.instance);
                attempt.current = null;
              },
            }))}
            style={{ flex: 1 }}
          >
            <View style={{ flexDirection: "row", alignItems: "center", gap: space.xs, minHeight: 36, paddingHorizontal: space.sm }}>
              <T variant="subhead" numberOfLines={1} style={{ flexShrink: 1 }}>
                {choice.name || choice.provider.instance}
              </T>
              <Icon name="chevron.up.chevron.down" size={12} color={colors.muted} />
            </View>
          </MenuButton>
          <Button
            size="medium"
            role="prominent"
            busy={busy}
            onPress={() => {
              setBusy(true);
              setError("");
              attempt.current ??= randomUUID();
              rpc("threads.switch_continue", { thread_id: thread.id, provider: choice.provider, message_id: attempt.current })
                .then(() => loadThread(thread.id))
                .catch((e) => setError(errorText(e)))
                .finally(() => setBusy(false));
            }}
          >
            Switch and continue
          </Button>
        </View>
      )}
      <ErrorBanner error={error} />
    </Panel>
  );
}
