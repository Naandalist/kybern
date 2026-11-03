// Accounts: every agent account on the computer. From the phone you can make one
// the default, rename it or change its colour; signing in or out happens on the
// computer, where the browser and the agent's files are.
import { useEffect } from "react";
import { PROVIDER_DISPLAY_NAME, type AccountSummary, type ProviderKind } from "../../state/protocol";
import { ACCOUNT_COLORS, loadAccounts, makeDefaultAccount, updateAccount, useAccounts } from "../../state/accounts";
import { errorText, useApp } from "../../state/runtime";
import { useSettings } from "../../state/settings";
import { Alert } from "../../ui/Alert";
import { ListRow } from "../../ui/List";
import type { MenuItem } from "../../ui/menuModel";
import { Empty, ErrorBanner } from "../../ui/primitives";
import { ProviderMark } from "../../ui/ProviderMark";
import { showToast } from "../../ui/Toast";
import { Section, WithSettings } from "./common";
import { hasSignIn } from "../../../../../packages/kybern-client/src/machineAgents";
import { accountStatusLabel } from "./labels";

export function AccountsSettings() {
  return (
    <WithSettings>
      <AccountsBody />
    </WithSettings>
  );
}

const sentence = (word: string) => word.charAt(0).toUpperCase() + word.slice(1);

function AccountsBody() {
  const app = useApp();
  const { settings } = useSettings();
  const { accounts, loaded, unsupported, error } = useAccounts();
  useEffect(() => {
    void loadAccounts();
  }, [app.activeId]);
  // Oh My Pi, OpenCode and pi run on the providers set up inside them and have no account of their own.
  // They are listed only for an extra profile (its own folder) someone added, never with a sign-in state.
  const shown = accounts.filter((a) => hasSignIn(a.provider.kind) || (!!a.provider.instance && a.provider.instance !== "default"));
  const kinds = [...new Set(shown.map((a) => a.provider.kind))] as ProviderKind[];
  const fail = (what: string) => (e: unknown) => showToast(`${what} ${errorText(e)}`);
  const rename = (account: AccountSummary) =>
    Alert.prompt(
      `Rename “${account.name}”`,
      undefined,
      [
        { text: "Cancel", style: "cancel" },
        {
          text: "Save",
          onPress: (value?: string) => {
            const name = value?.trim();
            if (!name || name === account.name) return;
            updateAccount(account, { name }).catch(fail("Unable to rename the account."));
          },
        },
      ],
      "plain-text",
      account.name,
    );
  const menu = (account: AccountSummary): MenuItem[] => [
    ...(account.is_default || !settings
      ? []
      : [
          {
            title: "Make default",
            icon: "checkmark.circle" as const,
            onPress: () =>
              void makeDefaultAccount(settings, account).then((saved) => {
                if (saved)
                  showToast(`${account.name} is now the default for ${PROVIDER_DISPLAY_NAME[account.provider.kind]}. Running turns keep their account.`);
              }),
          },
        ]),
    { title: "Rename…", icon: "pencil", onPress: () => rename(account) },
    {
      title: "Colour",
      icon: "circle",
      children: ACCOUNT_COLORS.map((color) => ({
        title: sentence(color),
        checked: account.color === color,
        onPress: () => void updateAccount(account, { color }).catch(fail("Unable to change the colour.")),
      })),
    },
  ];
  if (unsupported)
    return (
      <Section first footer="Update Kybern on your computer to manage accounts from your phone.">
        <ListRow title="Accounts aren't available" leading="person.crop.circle" />
      </Section>
    );
  return (
    <>
      <ErrorBanner error={error} onRetry={() => void loadAccounts()} />
      {loaded && shown.length === 0 ? (
        <Empty icon="person.crop.circle" title="No accounts yet" detail="Sign in to an agent on your computer and it shows up here." />
      ) : null}
      {kinds.map((kind, index) => (
        <Section
          key={kind}
          title={PROVIDER_DISPLAY_NAME[kind] ?? kind}
          first={index === 0}
          footer={index === kinds.length - 1 ? "Sign in, sign out and add accounts on your computer. Touch and hold an account for more." : null}
        >
          {shown
            .filter((a) => a.provider.kind === kind)
            .map((account) => (
              <ListRow
                key={account.provider.instance}
                title={account.name}
                subtitle={
                  hasSignIn(kind)
                    ? [account.identity?.email, account.identity?.plan, account.status === "signed_in" ? null : accountStatusLabel(account.status)]
                        .filter(Boolean)
                        .join(" · ") || accountStatusLabel(account.status)
                    : undefined
                }
                leading={<ProviderMark kind={kind} size={22} />}
                value={account.is_default ? "Default" : undefined}
                menu={menu(account)}
                menuTitle={account.name}
              />
            ))}
        </Section>
      ))}
    </>
  );
}
