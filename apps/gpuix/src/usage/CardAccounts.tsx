// The rail usage card's account half (`views/RailUsage.tsx` `AccountsUsageCard`): the default-account
// radio list with each account's tightest limit, "Needs sign-in" rows with their Sign in action,
// and the "Default changed" note. `NavRail.tsx` mounts it between the card header and the limits.
import { useEffect, useRef, useState } from 'react'
import { accountNeedsSignIn, accountsOfKind, limitsForAccount } from '../shared/lib/accountUi.ts'
import { CLI_INSTANCE, isCliInstance } from '../shared/lib/accounts.ts'
import { bindingLimit, PROVIDER_NAMES } from '../shared/lib/providerUsage.ts'
import type { AccountSummary, ProviderKind, ProviderLimits } from '../../../../packages/kybern-client/src/types.ts'
import { Icon } from '../icons.tsx'
import { Button } from '../kit/Button.tsx'
import { errorText } from '../nav/actions.ts'
import { N } from '../nav/glyphs.ts'
import { showDialog } from '../dialogs/store.ts'
import { toast } from '../shell/toasts.tsx'
import { Label, useTheme } from '../ui.tsx'
import { AccountMarkStack, LimitMeter } from './AccountMark.tsx'
import { askAccountLimits, refreshAccounts, updateProviderSettings, useAccountLimitEntries, useAccounts } from './accounts.ts'

/** What is left of an account's tightest limit, as a percentage, or null when nothing is known. */
function leftOf(entry: ProviderLimits | undefined, now: number): number | null {
  const binding = entry ? bindingLimit(entry.limits, now) : null
  return binding ? 100 - binding.used : null
}

/** The default account's limits (the card's meters), falling back to the provider's global entry. Opening re-reads accounts and their limits. */
export function useCardEntry(kind: ProviderKind, global: ProviderLimits): ProviderLimits {
  const entries = useAccountLimitEntries()
  const accounts = accountsOfKind(useAccounts(kind), kind)
  useEffect(() => {
    // Named accounts are read only while this card is open.
    askAccountLimits({ refresh: true })
    void refreshAccounts()
  }, [kind])
  const defaultAccount = accounts.find((account) => account.is_default) ?? accounts[0]
  return (defaultAccount ? limitsForAccount(defaultAccount, entries, [global]) : undefined) ?? global
}

export function CardAccounts({ kind, global }: { kind: ProviderKind; global: ProviderLimits }) {
  const t = useTheme()
  const entries = useAccountLimitEntries()
  const accounts = accountsOfKind(useAccounts(kind), kind)
  const [changed, setChanged] = useState(false)
  const changedTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined)
  useEffect(() => () => clearTimeout(changedTimer.current), [])
  const now = Date.now()
  const name = PROVIDER_NAMES[kind] ?? kind
  const choose = async (account: AccountSummary) => {
    if (account.is_default) return
    try {
      await updateProviderSettings(kind, (current) => ({ ...current, default_account: isCliInstance(account.provider.instance) ? null : account.provider.instance }))
      setChanged(true)
      clearTimeout(changedTimer.current)
      changedTimer.current = setTimeout(() => setChanged(false), 2000)
    } catch (error) {
      toast.error('Unable to change the default account', { description: `${errorText(error)} Check your connection, then try again.` })
    }
  }
  return <>
    {accounts.length > 1 && <div testId={`rail-accounts-${kind}`} role="radiogroup" aria-label={`Default ${name} account`} style={{ display: 'flex', flexDirection: 'column', gap: 1, marginLeft: -6, marginRight: -6 }}>
      {accounts.map((account) => <CardAccountRow key={account.provider.instance} account={account} left={leftOf(limitsForAccount(account, entries, [global]), now)} onChoose={() => void choose(account)} />)}
    </div>}
    {changed && <div role="status" style={{ display: 'flex' }}><Label color={t.textSecondary} size={13} lineHeight={19.5}>Default changed</Label></div>}
  </>
}

function CardAccountRow({ account, left, onChoose }: { account: AccountSummary; left: number | null; onChoose: () => void }) {
  const t = useTheme()
  const kind = account.provider.kind
  const cli = account.provider.instance === CLI_INSTANCE
  // `accountNeedsSignIn` leaves out the CLI account and the agents without a sign-in (omp, OpenCode, pi).
  const needsSignIn = accountNeedsSignIn(account)
  const email = account.identity?.email ?? account.identity?.plan ?? null
  const body = <>
    <AccountMarkStack kind={kind} color={cli ? null : account.color} size={14} />
    <div style={{ display: 'flex', flexDirection: 'column', flexGrow: 1, flexShrink: 1, minWidth: 0 }}>
      <Label color={t.text} size={14} lineHeight={21} truncate>{account.name}</Label>
      {email && <Label color={t.textSecondary} size={13} lineHeight={19.5} truncate>{email}</Label>}
    </div>
  </>
  const row = { display: 'flex', alignItems: 'center', gap: 10, paddingLeft: 6, paddingRight: 6, paddingTop: 6, paddingBottom: 6, borderRadius: 8, ...(account.is_default ? { backgroundColor: t.buttonSecondary } : {}) } as const
  if (needsSignIn) {
    return <div testId={`rail-account-${account.provider.instance}`} style={row}>
      <div role="radio" aria-checked={false} aria-disabled style={{ display: 'flex', alignItems: 'center', gap: 10, flexGrow: 1, flexShrink: 1, minWidth: 0, opacity: 0.7 }}>{body}</div>
      <Label color={t.warning} size={13} lineHeight={19.5}>Needs sign-in</Label>
      <Button testId={`rail-account-signin-${account.provider.instance}`} size="sm" onClick={() => showDialog({ kind: 'account', provider: kind, instance: account.provider.instance })}>Sign in</Button>
    </div>
  }
  return <div testId={`rail-account-${account.provider.instance}`} role="radio" aria-checked={account.is_default} aria-label={account.name} tabIndex={0} onClick={onChoose} onKeyDown={(e) => { if (e.key === 'enter' || e.key === 'space') onChoose() }} style={{ ...row, cursor: 'pointer', ...(account.is_default ? {} : { hover: { backgroundColor: t.buttonSecondaryHover } }) }}>
    {body}
    <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'flex-end', flexShrink: 0, minWidth: 48, gap: 4 }}>
      {left !== null ? <><Label color={t.textSecondary} size={13} lineHeight={13}>{`${Math.round(left)}% left`}</Label><div style={{ width: 48 }}><LimitMeter left={left} pace={null} label={`${account.name} left`} /></div></> : <Label color={t.textSecondary} size={13} lineHeight={19.5}>—</Label>}
    </div>
    <div style={{ display: 'flex', width: 14, height: 14, flexShrink: 0 }}>{account.is_default && <Icon source={N.check} size={14} color={t.text} />}</div>
  </div>
}
