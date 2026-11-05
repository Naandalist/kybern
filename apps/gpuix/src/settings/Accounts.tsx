// Settings › Accounts (`views/settings/AccountsSettings.tsx`): every signed-in account of each
// installed agent, with its email, plan, usage and status. The CLI account (the agent's own
// sign-in) comes first; named accounts follow in creation order.
import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { ACCOUNT_COLORS, ACCOUNT_COLOR_LABELS, isAccountColor, isCliInstance, isNameValid, withoutSignIn } from '../shared/lib/accounts.ts'
import { bindingLimit, limitLabel, PROVIDER_NAMES } from '../shared/lib/providerUsage.ts'
import type { AccountSummary, ProviderKind, ProviderStatus } from '../../../../packages/kybern-client/src/types.ts'
import { Icon } from '../icons.tsx'
import { goPage } from '../nav/history.ts'
import { N } from '../nav/glyphs.ts'
import { errorText } from '../nav/actions.ts'
import { Dialog, Menu, MenuItem, MenuSeparator, MenuSub, Tip } from '../nav/overlay.tsx'
import { useNav } from '../nav/store.ts'
import { showDialog } from '../dialogs/store.ts'
import { CloseX } from '../dialogs/Account.tsx'
import { activeEnvironment } from '../shell/environment.ts'
import { toast } from '../shell/toasts.tsx'
import { copyText } from '../transcript/store.ts'
import { getClient, useApp } from '../state.ts'
import { motion, useGpuix, type PublicInstance } from '@gpuix/react'
import { nativeCapabilities } from '../native/api.ts'
import { EASE } from '../motion/kit.tsx'
import { ink, Label, ProviderMark, REDUCED_MOTION, useTheme } from '../ui.tsx'
import { GlassEdge } from '../kit/GlassEdge.tsx'
import { settingsCardStyle, useGlass } from '../platform/glass.ts'
import { FONT_MONO } from '../theme.ts'
import { markdownTheme } from '../transcript/kit.tsx'
import { AccountAvatar, AccountDot, AccountUsageRing } from '../usage/AccountMark.tsx'
import { askAccountLimits, makeDefaultAccount, refreshAccounts, setUsageAnchor, updateProviderSettings, useAccountFlash, useAccountLimitEntries, useAccounts, useAccountsFeed, useAccountsLegacy, useAccountsLoaded } from '../usage/accounts.ts'
import { limitsForAccount } from '../shared/lib/accountUi.ts'
import { Checkbox } from '../kit/Checkbox.tsx'
import { Skeleton } from '../kit/Skeleton.tsx'
import { PATCHED_NATIVE } from '../native/local-build.ts'
import { DURATION } from '../motion/tokens.ts'
import { PageBlock, selectOwnsEscape, useSettingsModal } from './controls.tsx'
import { Button } from '../kit/Button.tsx'
import { IconButton } from '../kit/IconButton.tsx'
import { S } from './glyphs.ts'
import { openSettingsTab } from './SettingsScreen.tsx'
import { useSettings } from './store.ts'
import { requestAntigravitySignIn } from '../antigravity/notice.ts'
import { LoginBrokerSection } from './LoginBroker.tsx'

/** Skeleton rows fade in after 150 ms (the fade's own `delay`, no timer), so a fast answer never flashes them. */
const SKELETON_DELAY = PATCHED_NATIVE ? { initial: { opacity: 0 }, animate: { opacity: 1 }, transition: { delay: 0.15, duration: DURATION.quick, ease: [0.23, 1, 0.32, 1] as [number, number, number, number] } } : undefined

/** What to run in a terminal to sign the CLI account in again; Kybern picks the sign-in up on its own. */
const CLI_SIGN_IN_COMMAND: Record<ProviderKind, string> = {
  'claude-code': 'claude auth login',
  codex: 'codex login',
  cursor: 'cursor-agent login',
  omp: 'omp login',
  opencode: 'opencode auth login',
  pi: 'pi',
  antigravity: 'agy',
  'grok-build': 'grok login',
}

/** Agents whose identity the daemon can read; others keep an unknown status without a loading placeholder. */
const IDENTITY_KINDS: ProviderKind[] = ['claude-code', 'codex', 'cursor', 'antigravity']

const agentLabel = (kind: ProviderKind, providers: ProviderStatus[]) => providers.find((p) => p.kind === kind)?.display_name ?? PROVIDER_NAMES[kind] ?? kind

function openAddAccount(request: { provider?: ProviderKind; instance?: string } = {}) { showDialog({ kind: 'account', ...request }) }

/** The heading's end-aligned action. Hidden for older daemons, which cannot add accounts from the browser. */
export function AccountsHeaderAction() {
  const t = useTheme()
  const legacy = useAccountsLegacy()
  if (legacy) return null
  return <Button testId="account-add" icon={N.plus} label="Add account" onClick={() => openAddAccount()}>Add account</Button>
}

export function Accounts() {
  const t = useTheme()
  const providers = useNav((s) => s.providers)
  const providersLoaded = useNav((s) => s.providersLoaded)
  const projects = useApp((s) => s.projects)
  const { settings } = useSettings()
  const accounts = useAccounts()
  const legacy = useAccountsLegacy()
  const loaded = useAccountsLoaded()
  const [removing, setRemoving] = useState<AccountSummary | null>(null)
  const [assigning, setAssigning] = useState<AccountSummary | null>(null)
  const host = activeEnvironment().name
  useSettingsModal(!!removing || !!assigning)
  useAccountsFeed()
  // Read accounts and their usage while this tab is open.
  useEffect(() => { void refreshAccounts(); askAccountLimits() }, [])
  // omp, OpenCode and pi have no account: they are listed only to manage a profile (its own folder) added
  // before Kybern stopped offering them, so nothing a user set up disappears.
  const installed = providers.filter((p) => p.available && accounts.some((a) => a.provider.kind === p.kind && (!withoutSignIn(p.kind) || !isCliInstance(a.provider.instance))))
  const missing = providers.filter((p) => !p.available && !withoutSignIn(p.kind))
  return <>
    {legacy && <PageBlock><Note>{`Update Kybern on ${host} to sign in from the browser.`}</Note></PageBlock>}
    {!legacy && <LoginBrokerSection />}
    {installed.map((provider) => <AgentSection key={provider.kind} provider={provider} accounts={accounts.filter((a) => a.provider.kind === provider.kind)} readOnly={legacy} onRemove={setRemoving} onAssign={setAssigning} />)}
    {!(loaded && providersLoaded) && installed.length === 0 && <PageBlock><div testId="accounts-loading" role="status" paintTransform={SKELETON_DELAY} style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>{[0, 1].map((i) => <Skeleton key={i} height={56} radius={12} />)}</div></PageBlock>}
    {missing.length > 0 && <PageBlock><div style={{ display: 'flex', flexWrap: 'wrap', alignItems: 'baseline', paddingBottom: 30 }}><Note>{`${listAgents(missing)} ${missing.length === 1 ? 'isn’t' : 'aren’t'} installed. Set them up in `}</Note><TextLink testId="accounts-agent-providers" onClick={() => openSettingsTab('agents')}>Agent providers</TextLink><Note>.</Note></div></PageBlock>}
    {removing && <RemoveDialog account={removing} onClose={() => setRemoving(null)} />}
    {assigning && <ProjectsDialog account={assigning} accounts={accounts} projects={projects} projectAccounts={settings?.providers[assigning.provider.kind]?.project_accounts} onClose={() => setAssigning(null)} />}
  </>
}

function Note({ children }: { children: string }) {
  const t = useTheme()
  return <div style={{ display: 'flex', flexDirection: 'column', minWidth: 0 }}><Label color={t.textSecondary} size={12} lineHeight={19.2}>{children}</Label></div>
}
function TextLink({ children, onClick, testId }: { children: string; onClick: () => void; testId?: string }) {
  const t = useTheme()
  return <div testId={testId} role="button" tabIndex={0} onClick={onClick} onKeyDown={(e) => { if (e.key === 'enter' || e.key === 'space') onClick() }} style={{ display: 'flex', cursor: 'pointer' }}><Label color={t.textSecondary} size={12} lineHeight={19.2} style={{ textDecoration: 'underline' }}>{children}</Label></div>
}

function listAgents(missing: ProviderStatus[]): string {
  const names = missing.map((p) => p.display_name)
  return typeof Intl !== 'undefined' && 'ListFormat' in Intl ? new Intl.ListFormat('en', { style: 'long', type: 'conjunction' }).format(names) : names.join(', ')
}

function AgentSection({ provider, accounts, readOnly, onRemove, onAssign }: { provider: ProviderStatus; accounts: AccountSummary[]; readOnly: boolean; onRemove: (account: AccountSummary) => void; onAssign: (account: AccountSummary) => void }) {
  const t = useTheme()
  const g = useGlass()
  return <PageBlock><div testId={`accounts-${provider.kind}`} style={{ display: 'flex', flexDirection: 'column', gap: 12, paddingBottom: 30 }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 6, paddingLeft: 2 }}><ProviderMark kind={provider.kind} size={14} /><Label color={t.text} size={14} lineHeight={19.6} weight={600}>{provider.display_name}</Label></div>
    {!readOnly && withoutSignIn(provider.kind) && <div style={{ display: 'flex' }}><Note>{`${provider.display_name} uses the providers set up in it. Each profile below keeps its own setup and sessions in its own folder.`}</Note></div>}
    {!readOnly && !withoutSignIn(provider.kind) && accounts.length > 1 && <div style={{ display: 'flex' }}><Note>Accounts share skills and installed plugin files. Sign-ins, sessions and connections stay separate. Conflicting local files are kept in backups.</Note></div>}
    <div style={{ display: 'flex', flexDirection: 'column', ...settingsCardStyle(t, g), overflow: 'hidden' }}><GlassEdge radius={12} color={g.highlight} />
      {accounts.map((account, index) => <AccountRow key={account.provider.instance} first={index === 0} account={account} readOnly={readOnly} onRemove={onRemove} onAssign={onAssign} />)}
      {!readOnly && provider.kind !== 'antigravity' && !withoutSignIn(provider.kind) && <div style={{ display: 'flex', alignItems: 'center', minHeight: 44, paddingLeft: 12, paddingRight: 12, borderTopWidth: accounts.length ? 1 : 0, borderColor: t.v('--app-settings-card-border') }}><Button testId={`account-add-${provider.kind}`} variant="ghost" icon={N.plus} label={`Add ${provider.display_name} account`} onClick={() => openAddAccount({ provider: provider.kind })}>{`Add ${provider.display_name} account`}</Button></div>}
    </div>
  </div></PageBlock>
}

// ── Row ──────────────────────────────────────────────────────────────────────

function AccountRow({ account, first, readOnly, onRemove, onAssign }: { account: AccountSummary; first: boolean; readOnly: boolean; onRemove: (account: AccountSummary) => void; onAssign: (account: AccountSummary) => void }) {
  const t = useTheme()
  const { kind, instance } = account.provider
  const cli = isCliInstance(instance)
  const flash = useAccountFlash(kind, instance)
  const [renaming, setRenaming] = useState(false)
  const email = account.identity?.email
  const plan = account.identity?.plan
  const projectCount = account.projects.length
  const through = account.broker ? (account.broker.state === 'borrowing' ? `Signs in through ${account.broker.machine_name}` : account.broker.message ?? `Signs in through ${account.broker.machine_name}`) : null
  const detail = [through ?? email ?? (withoutSignIn(kind) ? null : 'Email unavailable'), through ? null : plan, projectCount > 0 ? (projectCount === 1 ? '1 project' : `${projectCount} projects`) : null, cli ? 'Same as your terminal' : null].filter(Boolean).join(' · ')
  return <div testId={`account-row-${kind}-${instance}`} style={{ position: 'relative', display: 'flex', alignItems: 'center', gap: 12, minHeight: 56, paddingLeft: 18, paddingRight: 18, paddingTop: 12, paddingBottom: 12, borderTopWidth: first ? 0 : 1, borderColor: t.v('--app-settings-card-border') }}>
    {flash && <FlashOverlay />}
    <AccountAvatar kind={kind} color={account.color} instance={instance} size={32} />
    <div style={{ display: 'flex', flexDirection: 'column', flexGrow: 1, flexShrink: 1, minWidth: 0 }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 8, minWidth: 0 }}>
        {renaming ? <RenameField account={account} onDone={() => setRenaming(false)} /> : <Label color={t.text} size={14} lineHeight={21} weight={500} truncate>{account.name}</Label>}
        {account.is_default && !renaming && <div style={{ display: 'flex', alignItems: 'center', height: 16, paddingLeft: 3, paddingRight: 3, borderRadius: 4, backgroundColor: t.buttonSecondary, flexShrink: 0 }}><Label color={t.textSecondary} size={11} lineHeight={14} weight={500}>Default</Label></div>}
      </div>
      <Label color={t.textSecondary} size={14} lineHeight={21.7} truncate>{detail}</Label>
    </div>
    <UsageCell account={account} />
    <StatusCell account={account} cli={cli} />
    {!readOnly && <AccountMenu account={account} onRename={() => setRenaming(true)} onRemove={onRemove} onAssign={onAssign} />}
  </div>
}

/** A row that was just added flashes once as its accent background fades out (`account-row-flash`, 600 ms). */
function FlashOverlay() {
  const t = useTheme()
  if (REDUCED_MOTION) return null
  return <motion.div initial={{ opacity: 1 }} animate={{ opacity: 0 }} transition={{ duration: 0.6, ease: [...EASE.smoothOut] as [number, number, number, number] }} style={{ position: 'absolute', left: 0, top: 0, right: 0, bottom: 0, backgroundColor: t.v('--color-background-accent'), pointerEvents: 'none' }} />
}

function RenameField({ account, onDone }: { account: AccountSummary; onDone: () => void }) {
  const t = useTheme()
  const [draft, setDraft] = useState(account.name)
  const [error, setError] = useState(false)
  const canceled = useRef(false)
  const saving = useRef(false)
  const save = async () => {
    const client = getClient()
    if (canceled.current || saving.current || !client) return
    const name = draft.trim()
    if (name === account.name) return onDone()
    if (!isNameValid(name)) { setError(true); return }
    saving.current = true
    try {
      await client.call('providers.accounts.update', { kind: account.provider.kind, instance: account.provider.instance, name })
      await refreshAccounts()
      toast.success(`Renamed to ${name}`)
      onDone()
    } catch (problem) {
      saving.current = false
      toast.error('Unable to rename the account', { description: `${errorText(problem)} Check the name, then try again.` })
    }
  }
  // The desktop selects the whole name on focus and darkens the border to foreground/30.
  const { renderer } = useGpuix()
  const input = useRef<PublicInstance>(null)
  const [focused, setFocused] = useState(false)
  const selectAll = () => { if (renderer && input.current) try { nativeCapabilities(renderer).setInputSelection(input.current.id, 0, draft.length, false) } catch { /* Older addons keep the caret at the end. */ } }
  return <div style={{ display: 'flex', flexDirection: 'column', flexGrow: 1, minWidth: 0 }}>
    <input ref={input} testId="account-rename" aria-label={`${account.name} name`} autoFocus value={draft} onFocus={() => { setFocused(true); selectAll() }} onChange={(e) => { setDraft(e.value ?? ''); setError(false) }} onBlur={() => { setFocused(false); void save() }} onSubmit={() => void save()} onKeyDown={(e) => { if (e.key === 'escape') { canceled.current = true; onDone() } }} style={{ width: '100%', height: 28, minWidth: 0, borderWidth: 1, borderColor: error ? t.v('--destructive') : focused ? ink(t, .3) : t.border, borderRadius: 8, paddingLeft: 10, paddingRight: 10, fontSize: 14, color: t.text, backgroundColor: t.inputGroupFill }} />
    {error && <Label color={t.v('--destructive')} size={13} lineHeight={19.5} style={{ marginTop: 4 }}>Enter a name up to 120 characters.</Label>}
  </div>
}

function UsageCell({ account }: { account: AccountSummary }) {
  const t = useTheme()
  const { kind, instance } = account.provider
  const entries = useAccountLimitEntries()
  const global = useNav((s) => s.limits)
  // The default account's limits are the global entry, labelled only once the agent has named accounts.
  const entry = limitsForAccount(account, entries, global)
  const binding = entry ? bindingLimit(entry.limits, Date.now()) : null
  if (!binding) return null
  const left = Math.max(0, Math.min(100, Math.round(100 - binding.used)))
  const window = limitLabel(binding.limit).toLowerCase()
  return <Tip label={`${left}% of the ${window} limit left · Open usage`} side="top" size={64}>
    <Button testId={`account-usage-${kind}-${instance}`} variant="ghost" size="sm" label={`${left}% left`} onClick={() => { setUsageAnchor(kind, instance); goPage('usage') }}>
      <AccountUsageRing left={left} />
      <Label color={t.textSecondary} size={12} lineHeight={16} weight={500}>{`${left}%`}</Label>
    </Button>
  </Tip>
}

function StatusCell({ account, cli }: { account: AccountSummary; cli: boolean }) {
  const t = useTheme()
  const { kind, instance } = account.provider
  const legacy = useAccountsLegacy()
  const wrap = (children: ReactNode) => <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'flex-end', gap: 8, minWidth: 104, flexShrink: 0 }}>{children}</div>
  const muted = (text: string) => <Label color={t.textSecondary} size={13} lineHeight={19.5}>{text}</Label>
  if (withoutSignIn(kind)) return wrap(null)
  const signIn = kind === 'antigravity' ? <Button testId="account-signin-antigravity" onClick={() => requestAntigravitySignIn(() => openAddAccount({ provider: kind }))}>Sign in</Button> : cli ? <HowToSignIn kind={kind} /> : <Button testId={`account-signin-${kind}-${instance}`} onClick={() => openAddAccount({ provider: kind, instance })}>Sign in</Button>
  switch (account.status) {
    case 'signed_in': return wrap(muted('Signed in'))
    case 'needs_sign_in': return wrap(<><div style={{ display: 'flex', alignItems: 'center', gap: 4 }}><Icon source={S.triangleAlert} size={12} color={t.warning} /><Label color={t.warning} size={13} lineHeight={19.5}>Needs sign-in</Label></div>{signIn}</>)
    case 'signed_out': return wrap(<>{muted('Signed out')}{signIn}</>)
    default:
      if (legacy) return wrap(cli ? null : signIn)
      if (account.identity) return wrap(muted('Signed in'))
      return IDENTITY_KINDS.includes(kind) ? wrap(<div role="status" aria-label="Checking the sign-in" style={{ width: 56, height: 10, borderRadius: 4, backgroundColor: t.buttonSecondary }} />) : wrap(null)
  }
}

function HowToSignIn({ kind }: { kind: ProviderKind }) {
  const t = useTheme()
  const command = CLI_SIGN_IN_COMMAND[kind]
  const [done, setDone] = useState(false)
  useEffect(() => { if (!done) return; const timer = setTimeout(() => setDone(false), 1400); return () => clearTimeout(timer) }, [done])
  // Escape closes the menu only, never the Settings page behind it (the desktop skips Escape from a `role="menu"`).
  const [open, setOpen] = useState(false)
  useSettingsModal(open)
  const description = useMemo(() => howToTheme(t), [t])
  const copy = () => { void copyText(command); setDone(true) }
  // `PopoverPopup` (`w-72`, 18px radius, 16px padding): title, a description with the command as inline code, then
  // the command on its own row with `CopyButton` (24px, icon only).
  return <Menu testId={`account-how-${kind}`} align="end" width={288} fit="switch" panel={{ borderRadius: 18, padding: 16 }} onOpenChange={setOpen} trigger={() => <Button>How to sign in</Button>}>
    <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
      <Label color={t.text} size={14} lineHeight={21} weight={500}>How to sign in</Label>
      <markdown style={{ userSelect: 'text' }} source={`Run \`${command}\` in your terminal. Kybern picks it up automatically.`} theme={description} />
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 8, height: 34, paddingLeft: 10, paddingRight: 4, borderWidth: 1, borderColor: t.border, borderRadius: 10 }}>
        <Label color={t.text} size={13} lineHeight={19.5} font={FONT_MONO} truncate>{command}</Label>
        <IconButton testId="account-copy-command" size="sm" icon={done ? N.check : S.copy} label={done ? 'Copied' : 'Copy command'} color={done ? t.v('--success') : undefined} onClick={copy} />
      </div>
    </div>
  </Menu>
}

/** `PopoverDescription`: 14/20 muted text; its `<code>` is foreground on `button-secondary`, 4px radius, 2×4px padding, 0.92em. */
function howToTheme(t: ReturnType<typeof useTheme>) {
  const base = markdownTheme(t, 'muted')
  return { ...base, text: t.textSecondary, codeText: t.text, codeWash: t.buttonSecondary, markdown: { ...base.markdown, inlineCodePaddingX: 4, inlineCodePaddingY: 2, inlineCodeSizeRatio: 0.92 }, metrics: { ...base.metrics, mdTextSize: 14, mdLineHeight: 20, mdInlineCodeRadius: 4 } }
}

// ── Menu ─────────────────────────────────────────────────────────────────────

function AccountMenu({ account, onRename, onRemove, onAssign }: { account: AccountSummary; onRename: () => void; onRemove: (account: AccountSummary) => void; onAssign: (account: AccountSummary) => void }) {
  const t = useTheme()
  const { kind, instance } = account.provider
  const cli = isCliInstance(instance)
  const id = `${kind}-${instance}`
  const signOut = async () => {
    const client = getClient()
    if (!client) return
    try {
      await client.call('providers.accounts.sign_out', account.provider)
      await refreshAccounts()
      toast.success(`${account.name} signed out`)
    } catch (error) {
      toast.error(`Unable to sign ${account.name} out`, { description: `${errorText(error)} Check your connection, then try again.` })
    }
  }
  const setColor = async (color: string) => {
    const client = getClient()
    if (!client) return
    try {
      await client.call('providers.accounts.update', { kind, instance, color })
      await refreshAccounts()
    } catch (error) {
      toast.error('Unable to change the color', { description: `${errorText(error)} Check your connection, then try again.` })
    }
  }
  const [open, setOpen] = useState(false)
  useSettingsModal(open)
  return <Menu testId={`account-actions-${id}`} align="end" minWidth={192} onOpenChange={setOpen} trigger={() => <IconButton icon={N.ellipsis} label="Account actions" active={open} />}>
    {!cli && <MenuItem id="rename" testId={`account-actions-${id}-rename`} label="Rename…" onSelect={onRename} />}
    {!cli && !withoutSignIn(kind) && <MenuItem id="signin" testId={`account-actions-${id}-signin`} label="Sign in again" onSelect={() => openAddAccount({ provider: kind, instance })} />}
    {!cli && !withoutSignIn(kind) && account.can_sign_out && account.status === 'signed_in' && <MenuItem id="sign_out" testId={`account-actions-${id}-sign_out`} label="Sign out" onSelect={() => void signOut()} />}
    {!cli && <MenuSeparator />}
    {!account.is_default && <MenuItem id="default" testId={`account-actions-${id}-default`} label="Make default" onSelect={() => void makeDefaultAccount(account)} />}
    <MenuItem id="projects" testId={`account-actions-${id}-projects`} label="Use for projects…" onSelect={() => onAssign(account)} />
    {!cli && <MenuSub id="color" testId={`account-actions-${id}-color`} label="Color">
      {ACCOUNT_COLORS.map((color) => <MenuItem key={color} id={`color-${color}`} testId={`account-actions-${id}-color-${color}`} label={ACCOUNT_COLOR_LABELS[color]} checked={isAccountColor(account.color) && account.color === color} iconNode={<AccountDot color={color} size={8} />} onSelect={() => void setColor(color)} />)}
    </MenuSub>}
    {!cli && <MenuSeparator />}
    {!cli && <MenuItem id="remove" testId={`account-actions-${id}-remove`} label="Remove…" destructive onSelect={() => onRemove(account)} />}
  </Menu>
}

// ── Dialogs ──────────────────────────────────────────────────────────────────

function DialogShell({ children, onClose, testId, width = 448 }: { children: ReactNode; onClose: () => void; testId: string; width?: number }) {
  return <Dialog testId={testId} width={width} onClose={() => { if (!selectOwnsEscape()) onClose() }}>{children}</Dialog>
}
function DialogFooter({ children }: { children: ReactNode }) {
  return <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'flex-end', gap: 8, paddingLeft: 16, paddingRight: 16, paddingTop: 12, paddingBottom: 12 }}>{children}</div>
}

function RemoveDialog({ account, onClose }: { account: AccountSummary; onClose: () => void }) {
  const t = useTheme()
  const providers = useNav((s) => s.providers)
  const [busy, setBusy] = useState(false)
  const agent = agentLabel(account.provider.kind, providers)
  const remove = async () => {
    const client = getClient()
    if (!client) return
    setBusy(true)
    try {
      await client.call('providers.accounts.remove', account.provider)
      await refreshAccounts()
      toast.success(`${account.name} removed`)
      onClose()
    } catch (error) {
      // The daemon says when a turn is still running ("Work is running in 2 threads. Stop them, then remove it.").
      toast.error(`Unable to remove ${account.name}`, { description: errorText(error) })
    } finally {
      setBusy(false)
    }
  }
  return <DialogShell testId="account-remove" onClose={() => { if (!busy) onClose() }}>
    <div style={{ display: 'flex', flexDirection: 'column', gap: 6, paddingLeft: 16, paddingRight: 16, paddingTop: 16, paddingBottom: 8 }}>
      <Label color={t.text} size={18} lineHeight={22.5} weight={600}>{`Remove ${account.name}?`}</Label>
      <Label color={t.textSecondary} size={14} lineHeight={19.25}>{account.managed ? `Kybern signs ${account.name} out and deletes its folder. Threads that used ${account.name} switch to ${agent}’s default account on their next message.` : `Kybern stops using ${account.name}. The folder and its sign-in stay at ${account.directory ?? 'its folder'}.`}</Label>
    </div>
    <DialogFooter><Button testId="account-remove-cancel" size="lg" onClick={onClose}>Cancel</Button><Button testId="account-remove-confirm" size="lg" variant="destructive" disabled={busy} onClick={() => void remove()}>Remove account</Button></DialogFooter>
  </DialogShell>
}

function ProjectsDialog({ account, accounts, projects, projectAccounts, onClose }: { account: AccountSummary; accounts: AccountSummary[]; projects: { id: string; name: string; path: string }[]; projectAccounts: Record<string, string> | undefined; onClose: () => void }) {
  const t = useTheme()
  const { kind, instance } = account.provider
  const stored = projectAccounts ?? {}
  const [checked, setChecked] = useState(() => new Set(projects.filter((p) => stored[p.path] === instance).map((p) => p.path)))
  const [saving, setSaving] = useState(false)
  const otherName = (path: string) => {
    const other = stored[path]
    if (!other || other === instance) return null
    return accounts.find((a) => a.provider.kind === kind && a.provider.instance === other)?.name ?? null
  }
  const sorted = useMemo(() => [...projects].sort((a, b) => a.name.localeCompare(b.name)), [projects])
  const save = async () => {
    setSaving(true)
    try {
      await updateProviderSettings(kind, (current) => {
        const next = { ...current.project_accounts }
        for (const project of projects) {
          if (checked.has(project.path)) next[project.path] = instance
          else if (next[project.path] === instance) delete next[project.path]
        }
        return { ...current, project_accounts: next }
      })
      onClose()
    } catch (error) {
      toast.error('Unable to save the projects', { description: `${errorText(error)} Check your connection, then try again.` })
    } finally {
      setSaving(false)
    }
  }
  const toggle = (path: string) => setChecked((prev) => { const next = new Set(prev); if (next.has(path)) next.delete(path); else next.add(path); return next })
  return <DialogShell testId="account-projects" onClose={onClose}>
    <div style={{ display: 'flex', flexDirection: 'column', gap: 6, paddingLeft: 16, paddingRight: 48, paddingTop: 16, paddingBottom: 4 }}>
      <Label color={t.text} size={18} lineHeight={22.5} weight={600}>{`Use ${account.name} for projects`}</Label>
      <Label color={t.textSecondary} size={14} lineHeight={19.25}>{`These projects use ${account.name} instead of the default. Their worktrees do too.`}</Label>
    </div>
    <CloseX onClose={onClose} />
    <div testId="account-projects-list" aria-label="Projects" style={{ display: 'flex', flexDirection: 'column', gap: 2, maxHeight: 288, overflowY: 'scroll', paddingLeft: 16, paddingRight: 16, paddingTop: 4, paddingBottom: 12 }}>
      {sorted.map((project) => {
        const other = otherName(project.path)
        return <div key={project.id} testId={`account-project-${project.id}`} role="checkbox" aria-checked={checked.has(project.path)} aria-label={project.name} tabIndex={0} onClick={() => toggle(project.path)} onKeyDown={(e) => { if (e.key === 'enter' || e.key === 'space') toggle(project.path) }} style={{ display: 'flex', alignItems: 'center', gap: 10, minHeight: 36, paddingLeft: 8, paddingRight: 8, paddingTop: 6, paddingBottom: 6, borderRadius: 8, cursor: 'pointer', hover: { backgroundColor: t.hover } }}>
          <Checkbox checked={checked.has(project.path)} aria-label={project.name} style={{ pointerEvents: 'none' }} />
          <div style={{ display: 'flex', flexDirection: 'column', flexGrow: 1, minWidth: 0 }}><Label color={t.text} size={14} lineHeight={21} truncate>{project.name}</Label><Label color={t.textSecondary} size={13} lineHeight={19.5} truncate>{project.path}</Label></div>
          {other && <Label color={t.textSecondary} size={13} lineHeight={19.5}>{`Uses ${other}`}</Label>}
        </div>
      })}
      {sorted.length === 0 && <div style={{ paddingLeft: 8, paddingRight: 8, paddingTop: 12, paddingBottom: 12 }}><Label color={t.textSecondary} size={14} lineHeight={21}>No projects yet. Add a project from the sidebar.</Label></div>}
    </div>
    <DialogFooter><Button testId="account-projects-cancel" size="lg" variant="ghost" onClick={onClose}>Cancel</Button><Button testId="account-projects-save" size="lg" variant="primary" disabled={saving} onClick={() => void save()}>Save</Button></DialogFooter>
  </DialogShell>
}

