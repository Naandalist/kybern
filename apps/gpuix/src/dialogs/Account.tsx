// The Add account sheet (`components/kybern/accounts/AddAccountSheet.tsx`): one dialog whose body
// moves between steps (pick, waiting, paste, device, terminal, success, duplicate, error) as a
// daemon-driven sign-in runs (`providers.accounts.login.*`). The common path is one browser trip:
// pick an agent, approve in the browser, come back to a check mark, a prefilled name and a colour.
// It also signs an existing account in again.
import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { motion } from '@gpuix/react'
import {
  ACCOUNT_COLORS,
  ACCOUNT_COLOR_LABELS,
  accountDisplayName,
  accountFor,
  initialLoginMode,
  isNameValid,
  loginHost,
  remoteNote,
  shouldFinishOnClose,
  stepFor,
  successDefaults,
  supportsBrowserLogin,
  supportsDeviceCode,
  supportsPasteCode,
  withoutSignIn,
  type AccountColor,
  type SheetStep,
} from '../shared/lib/accounts.ts'
import { PROVIDER_NAMES } from '../shared/lib/providerUsage.ts'
import type { AccountLoginMode, ProviderKind } from '../../../../packages/kybern-client/src/types.ts'
import { Icon } from '../icons.tsx'
import { MatrixLoader } from '../MatrixLoader.tsx'
import { DURATION, EASE, Rise } from '../motion/kit.tsx'
import { N } from '../nav/glyphs.ts'
import { Dialog } from '../nav/overlay.tsx'
import { setNav } from '../nav/store.ts'
import { errorText } from '../nav/actions.ts'
import { useNav } from '../nav/store.ts'
import { Button } from '../kit/Button.tsx'
import { IconButton } from '../kit/IconButton.tsx'
import { TextField } from '../kit/Field.tsx'
import { useControls } from '../kit/controls.ts'
import { Chevron } from '../motion/Chevron.tsx'
import { Switch } from '../kit/Switch.tsx'
import { openExternal, pickFolder } from '../settings/external.ts'
import { S } from '../settings/glyphs.ts'
import { profileById } from '../environments/registry.ts'
import { activeEnvironment } from '../shell/environment.ts'
import { toast } from '../shell/toasts.tsx'
import { copyText } from '../transcript/store.ts'
import { Screen as TerminalScreen } from '../terminal/Terminal.tsx'
import { getClient } from '../state.ts'
import { ink, Label, ProviderMark, REDUCED_MOTION, useTheme } from '../ui.tsx'
import { FONT_MONO, FONT_UI } from '../theme.ts'
import { AccountAvatar, accountColor } from '../usage/AccountMark.tsx'
import { flashAccount, refreshAccounts, useAccounts, useAccountsFeed, useAccountsLegacy } from '../usage/accounts.ts'
import { useSettings } from '../settings/store.ts'
import { useAccountLogin } from './useAccountLogin.ts'
import { SuccessCheck } from '../motion/SuccessCheck.tsx'

const TERMINAL_COPY: Partial<Record<ProviderKind, string>> = {
  opencode: 'OpenCode asks for an API key. Paste it into the prompt below.',
  pi: 'Pi asks which provider to sign in to. Follow its prompts below.',
  antigravity: 'Antigravity opens your browser to finish signing in with Google. Kybern checks the sign-in when it finishes.',
}

export function Account({ close, provider, instance }: { close: () => void; provider?: ProviderKind; instance?: string }) {
  const t = useTheme()
  useAccountsFeed()
  const providers = useNav((s) => s.providers)
  const legacy = useAccountsLegacy()
  const [kind, setKind] = useState<ProviderKind | undefined>(provider)
  const [folder, setFolder] = useState('')
  const [advancedOpen, setAdvancedOpen] = useState(false)
  const api = useAccountLogin()
  const apiRef = useRef(api)
  useEffect(() => { apiRef.current = api })
  const { login, failure, starting } = api
  // Signing in again: opened for an account, or "Sign in again to Work" from the duplicate step.
  const reauthInstance = login?.instance ?? instance
  const reauth = !!reauthInstance
  // Antigravity and Grok Build have one account per machine: no pick step, no name or colour, and nothing to add.
  const single = (kind ?? provider) === 'antigravity' || (kind ?? provider) === 'grok-build'
  const accounts = useAccounts(kind)
  const { settings } = useSettings()
  const environment = activeEnvironment()
  const remote = !environment.local
  const host = environment.name
  const transport: 'SSH' | 'Tailscale' = remote && profileById(environment.id).ssh ? 'SSH' : 'Tailscale'
  const agent = kind ? providers.find((p) => p.kind === kind)?.display_name ?? PROVIDER_NAMES[kind] ?? kind : ''
  const target = kind && reauthInstance ? accountFor(accounts, kind, reauthInstance) : undefined
  const targetName = accountDisplayName(reauthInstance, target?.name)

  // Success step values, prefilled from the daemon's suggestion.
  const usedColors = useMemo(() => accounts.map((a) => a.color), [accounts])
  const defaults = useMemo(() => (login && login.phase === 'signed_in' ? successDefaults(login, usedColors) : null), [login, usedColors])
  const [name, setName] = useState<string | null>(null)
  const [color, setColor] = useState<AccountColor | null>(null)
  const [makeDefault, setMakeDefault] = useState(false)
  const [finishError, setFinishError] = useState<string | null>(null)
  const [finishing, setFinishing] = useState(false)
  const [code, setCode] = useState('')
  const [codeShown, setCodeShown] = useState(false)
  // A code is single use: each refusal clears the field, like the desktop's remounted input.
  useEffect(() => { setCode('') }, [api.wrongCodes])
  const shownName = name ?? defaults?.name ?? ''
  const shownColor = color ?? defaults?.color ?? 'blue'
  const namedCount = accounts.filter((a) => a.provider.instance !== 'default').length
  const providerSettings = kind ? settings?.providers[kind] : undefined
  const forcedDefault = namedCount === 0 && !providerSettings?.default_account
  const defaultOn = forcedDefault || makeDefault

  const step: SheetStep | null = failure ? 'error' : api.legacyTerminal ? 'terminal' : (instance || single) && !login ? null : stepFor(login)
  const verifying = login?.phase === 'verifying'
  const modeFor = (k: ProviderKind): AccountLoginMode => initialLoginMode(k, remote)

  const begin = async (patch: { mode?: AccountLoginMode } = {}) => {
    if (!kind) return
    const mode = patch.mode ?? modeFor(kind)
    if (reauth && legacy) { await api.signInLegacy({ kind, instance: reauthInstance! }); return }
    await api.start({ kind, mode, ...(reauth ? { instance: reauthInstance } : {}), ...(!reauth && folder.trim() ? { directory: folder.trim() } : {}) })
  }

  // Signing in again starts straight away, with no pick step.
  const started = useRef(false)
  useEffect(() => {
    if (!(reauth || single) || !kind || started.current) return
    started.current = true
    void begin()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])
  useEffect(() => { if ((instance || single) && provider && !kind) setKind(provider) }, [instance, provider, kind, single])

  const finish = async (): Promise<boolean> => {
    const client = getClient()
    if (!login || !kind || !client) return false
    setFinishing(true)
    setFinishError(null)
    try {
      const result = await client.call('providers.accounts.login.finish', { id: login.id, name: shownName.trim(), color: shownColor, make_default: defaultOn })
      await refreshAccounts()
      flashAccount(result.kind, result.instance)
      toast.success(`${shownName.trim()} added`)
      return true
    } catch (error) {
      // Daemon errors already say what to do next (the name check says "Enter a name…").
      setFinishError(errorText(error))
      return false
    } finally {
      setFinishing(false)
    }
  }

  // Esc, the close button and the backdrop all arrive here. A sign-in that is still running is
  // canceled; one that already succeeded is saved, because signing in counted as consent.
  const stateRef = useRef({ login, shownName, shownColor, defaultOn })
  useEffect(() => { stateRef.current = { login, shownName, shownColor, defaultOn } })
  const dismissed = useRef(false)
  const dismiss = () => {
    if (dismissed.current) return
    dismissed.current = true
    const current = stateRef.current
    const client = getClient()
    if (single) {
      // The machine's one Antigravity sign-in is already saved by `agy`; there is nothing to name or finish.
      if (current.login?.phase === 'signed_in') void refreshAccounts()
      else void apiRef.current.cancel()
    } else if (client && shouldFinishOnClose(current.login) && current.login && isNameValid(current.shownName)) {
      const label = current.shownName.trim()
      void client.call('providers.accounts.login.finish', { id: current.login.id, name: label, color: current.shownColor, make_default: current.defaultOn }).then(
        async (result) => { await refreshAccounts(); flashAccount(result.kind, result.instance); toast.success(`${label} added`) },
        (error) => toast.error(`Unable to add ${label}`, { description: `${errorText(error)} Open Settings › Accounts to try again.` }),
      )
    } else {
      void apiRef.current.cancel()
      if (current.login?.phase === 'signed_in' && current.login.instance) void refreshAccounts()
    }
    close()
  }
  // Unmounting from outside (an environment switch) still cancels a running sign-in.
  useEffect(() => () => { if (!dismissed.current) void apiRef.current.cancel() }, [])

  // An older daemon has no login record: its sign-in ends when the terminal's process exits.
  const legacyId = api.legacyTerminal?.id
  useEffect(() => {
    const client = getClient()
    if (!legacyId || !client) return
    return client.onNotification('terminal.exited', (params) => { if ((params as { terminal_id?: string }).terminal_id === legacyId) { void refreshAccounts(); dismiss() } })
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [legacyId])
  const showTerminal = () => void api.restart({ mode: 'terminal' })
  const tryAgain = () => (reauth && legacy ? begin() : api.retry())

  let body: ReactNode
  let footer: ReactNode
  if (step === 'pick' || (!step && !reauth)) {
    const ready = !!kind && providers.some((p) => p.kind === kind && p.available)
    // Antigravity signs in with `agy` on its own sheet; omp has no account (it runs on the providers set up inside it).
    body = <PickStep providers={providers.filter((p) => p.kind !== 'antigravity' && p.kind !== 'grok-build' && !withoutSignIn(p.kind))} kind={kind} onKind={(next) => {
      const status = providers.find((p) => p.kind === next)
      if (status && !status.available) { setNav({ settingsOpen: true }); close(); void import('../settings/SettingsScreen.tsx').then((m) => m.openSettingsTab('agents')); return }
      setKind(next)
    }} folder={folder} onFolder={setFolder} advancedOpen={advancedOpen} onAdvancedOpen={setAdvancedOpen} remote={remote} />
    footer = <><Button size="lg" testId="account-cancel" variant="ghost" onClick={dismiss}>Cancel</Button><Button size="lg" testId="account-start" variant="primary" disabled={!ready || starting} onClick={() => void begin()}>{kind && supportsBrowserLogin(kind) && !remote ? 'Continue in browser' : 'Continue'}</Button></>
  } else if (step === 'waiting' && login && kind) {
    const url = loginHost(login.url)
    const canCode = supportsPasteCode(kind) || supportsDeviceCode(kind)
    const codeMode: AccountLoginMode = supportsDeviceCode(kind) ? 'device_code' : 'paste'
    body = <Body align="center" top={20}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 12 }}><AccountAvatar kind={kind} size={32} instance={reauthInstance} color={target?.color} /><MatrixLoader variant="orbit" dot={3} gap={2} /></div>
      <Title center>{reauth ? `Sign in again to ${targetName}` : 'Finish signing in in your browser'}</Title>
      {url && <Desc center>{`We opened ${url}. Come back here when you’re done.`}</Desc>}
      {verifying && <Checking />}
      <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 6, marginTop: 4 }}>
        {login.url && <QuietLink testId="account-reopen" onClick={() => void openExternal(login.url!).catch((e) => toast.error('Unable to open the sign-in page', { description: `${errorText(e)} Copy its address and open it in your browser.` }))}>Open browser again</QuietLink>}
        {canCode && !reauth && <div style={{ display: 'flex', alignItems: 'center', gap: 4 }}><Quiet>Browser on another device?</Quiet><QuietLink testId="account-code-instead" onClick={() => void api.restart({ mode: codeMode })}>{supportsDeviceCode(kind) ? 'Use a code instead' : 'Paste code instead'}</QuietLink></div>}
        {canCode && reauth && <QuietLink testId="account-code-instead" onClick={() => void api.restart({ mode: codeMode })}>{supportsDeviceCode(kind) ? 'Use a code instead' : 'Paste code instead'}</QuietLink>}
        <QuietLink testId="account-show-terminal" onClick={showTerminal}>Having trouble? Show terminal</QuietLink>
      </div>
    </Body>
    footer = <Button size="lg" testId="account-cancel" variant="ghost" onClick={dismiss}>Cancel</Button>
  } else if (step === 'paste' && login && kind) {
    const sending = api.codeState === 'sending' || verifying
    const submit = () => { if (code.trim() && !sending) void api.submitCode(code) }
    body = <PasteStep login={login} remote={remote} host={host} transport={transport} wrong={api.codeState === 'wrong'} code={code} onCode={setCode} shown={codeShown} onShown={setCodeShown} onSubmit={submit} onTerminal={showTerminal} />
    footer = <><Button size="lg" testId="account-cancel" variant="ghost" onClick={dismiss}>Cancel</Button><Button size="lg" testId="account-paste-submit" variant="primary" busy={sending} disabled={sending} onClick={submit}>Sign in</Button></>
  } else if (step === 'device' && login && kind) {
    const url = loginHost(login.url)
    body = <Body gap={12}>
      <Title pe>Enter this code in your browser</Title>
      {remote && <RemoteNote host={host} transport={transport} />}
      <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 12, paddingLeft: 16, paddingRight: 16, paddingTop: 20, paddingBottom: 20, borderWidth: 1, borderColor: t.border, borderRadius: 12 }}>
        {login.user_code ? <Label color={t.text} size={30} lineHeight={30} weight={500} font={FONT_MONO}>{login.user_code}</Label> : <div style={{ width: 192, height: 32, borderRadius: 6, backgroundColor: t.buttonSecondary }} />}
        <div style={{ display: 'flex', flexWrap: 'wrap', justifyContent: 'center', gap: 8 }}>
          {login.user_code && <CopyButton text={login.user_code} label="Copy code" />}
          {login.url && <Button testId="account-open-page" icon={S.externalLink} onClick={() => void openExternal(login.url!)}>{`Open ${url ?? 'sign-in page'}`}</Button>}
        </div>
      </div>
      <Label color={t.textSecondary} size={13} lineHeight={19.5} style={{ textAlign: 'center' }}>The code expires in 15 minutes.</Label>
      {verifying && <Checking />}
      <div style={{ display: 'flex', justifyContent: 'center' }}><QuietLink testId="account-show-terminal" onClick={showTerminal}>Having trouble? Show terminal</QuietLink></div>
    </Body>
    footer = <Button size="lg" testId="account-cancel" variant="ghost" onClick={dismiss}>Cancel</Button>
  } else if (step === 'terminal' && kind) {
    const info = api.legacyTerminal ?? login?.terminal
    body = <Body gap={12}>
      <Title>Finish in the terminal</Title>
      <Desc>{TERMINAL_COPY[kind] ?? `Follow ${agent}’s prompts below. Kybern checks the sign-in when it finishes.`}</Desc>
      {legacy && <Label color={t.textSecondary} size={13} lineHeight={19.5}>{`Update Kybern on ${host} to sign in from the browser.`}</Label>}
      <div testId="account-terminal" style={{ display: 'flex', flexDirection: 'column', height: 256, minWidth: 0, borderWidth: 1, borderColor: t.border, borderRadius: 8, overflow: 'hidden' }}>
        {info ? <TerminalScreen key={info.id} info={info} width={382} /> : <div style={{ flexGrow: 1, backgroundColor: t.buttonSecondary }} />}
      </div>
      {verifying && <Checking />}
    </Body>
    footer = <Button size="lg" testId="account-cancel" variant="ghost" onClick={dismiss}>Cancel</Button>
  } else if (step === 'success' && login && kind) {
    const email = login.identity?.email
    const meta = [[agent, login.identity?.plan].filter(Boolean).join(' '), login.identity?.organization].filter(Boolean).join(' · ')
    const emailChanged = reauth && !!login.previous_email && !!email && login.previous_email.toLowerCase() !== email.toLowerCase()
    body = <Body gap={16}>
      <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 8, paddingTop: 8 }}>
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'center', width: 40, height: 40, borderRadius: 20, backgroundColor: t.v('--success') + '1f' }}><SuccessCheck testId="account-success-check" size={20} color={t.v('--success')} /></div>
        <Title center>{email ? `Signed in as ${email}` : 'Signed in'}</Title>
        {meta && <Desc center>{meta}</Desc>}
        {emailChanged && <Label color={t.textSecondary} size={13} lineHeight={19.5} style={{ textAlign: 'center' }}>{`${targetName} was ${login.previous_email}. It now uses ${email}.`}</Label>}
      </div>
      {!reauth && !single && <div style={{ display: 'flex', flexDirection: 'column', gap: 16 }}>
        <div style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
          <Label color={t.text} size={14} lineHeight={21} weight={500}>Name</Label>
          <TextField testId="account-name" label="Name" size="lg" invalid={!isNameValid(shownName)} value={shownName} onChange={(value) => setName(value)} width="100%" onSubmit={() => { if (isNameValid(shownName)) void finish().then((ok) => { if (ok) { dismissed.current = true; close() } }) }} />
          {!isNameValid(shownName) && <Label color={t.v('--destructive')} size={13} lineHeight={19.5}>Enter a name up to 120 characters.</Label>}
        </div>
        <div style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
          <Label color={t.text} size={14} lineHeight={21} weight={500}>Color</Label>
          <ColorSwatches value={shownColor} onChange={setColor} />
        </div>
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 12 }}>
          <Label color={t.text} size={14} lineHeight={21} weight={500}>{`Use as default for ${agent}`}</Label>
          <Switch testId="account-default" aria-label={`Use as default for ${agent}`} checked={defaultOn} disabled={forcedDefault} onCheckedChange={setMakeDefault} />
        </div>
        {forcedDefault && <Label color={t.textSecondary} size={13} lineHeight={19.5} style={{ marginTop: -10 }}>Your first added account becomes the default. You can switch back to the CLI account in Settings › Accounts.</Label>}
      </div>}
      {finishError && <Label color={t.v('--destructive')} size={13} lineHeight={19.5}>{finishError}</Label>}
    </Body>
    footer = reauth || single
      ? <Button size="lg" testId="account-done" variant="primary" onClick={() => { void refreshAccounts(); dismissed.current = true; close() }}>Done</Button>
      : <Button size="lg" testId="account-finish" variant="primary" disabled={!isNameValid(shownName) || finishing} onClick={() => void finish().then((ok) => { if (ok) { dismissed.current = true; close() } })}>Add account</Button>
  } else if (step === 'duplicate' && login && kind) {
    const existing = accountFor(accounts, kind, login.duplicate_of)
    const isCli = login.duplicate_of === 'default'
    const existingName = accountDisplayName(login.duplicate_of, existing?.name)
    body = <Body align="center" top={16} gap={8}>
      <AccountAvatar kind={kind} size={32} instance={login.duplicate_of} color={existing?.color} />
      <Title center>{`Already added as ${existingName}`}</Title>
      <Desc center>{isCli ? `This is the account your ${agent} CLI already uses.` : `${login.identity?.email ?? 'This email'} is ${agent}’s ${existingName} account.`}</Desc>
    </Body>
    footer = isCli
      ? <Button size="lg" testId="account-done" variant="primary" onClick={dismiss}>Done</Button>
      : <><Button size="lg" testId="account-done" variant="ghost" onClick={dismiss}>Done</Button><Button size="lg" testId="account-reauth" variant="primary" onClick={() => void api.restart({ instance: login.duplicate_of, directory: undefined, upstream: undefined, mode: login.mode })}>{`Sign in again to ${existingName}`}</Button></>
  } else if (step === 'error') {
    const message = failure ?? login?.error ?? 'Sign-in didn’t finish. Try again, or show the terminal.'
    body = <Body align="center" top={16} gap={8}>
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'center', width: 40, height: 40, borderRadius: 20, backgroundColor: t.v('--destructive') + '1a' }}><Icon source={S.circleAlert} size={20} color={t.v('--destructive')} /></div>
      <Title>Unable to sign in</Title>
      <Desc center testId="account-error">{message}</Desc>
    </Body>
    footer = <><Button size="lg" testId="account-cancel" variant="ghost" onClick={dismiss}>Cancel</Button>{login?.mode !== 'terminal' && kind && !legacy && <Button size="lg" testId="account-show-terminal" onClick={showTerminal}>Show terminal</Button>}<Button size="lg" testId="account-retry" variant="primary" onClick={() => void tryAgain()}>Try again</Button></>
  } else {
    body = <Body><Title>{single ? `Sign in to ${agent || 'Antigravity'}` : 'Add an account'}</Title><div style={{ height: 64, marginTop: 12, borderRadius: 6, backgroundColor: t.buttonSecondary }} /></Body>
    footer = <Button size="lg" testId="account-cancel" variant="ghost" onClick={dismiss}>Cancel</Button>
  }
  return <Dialog onClose={dismiss} width={448} testId="add-account">
    <Rise key={step ?? 'none'} kind="row">{body}</Rise>
    {footer && <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'flex-end', gap: 8, paddingLeft: 16, paddingRight: 16, paddingTop: 12, paddingBottom: 12 }}>{footer}</div>}
    <CloseX onClose={dismiss} />
  </Dialog>
}

// ── Pieces ───────────────────────────────────────────────────────────────────

export function CloseX({ onClose }: { onClose: () => void }) {
  return <IconButton testId="dialog-close" icon={N.x} label="Close" size="lg" onClick={onClose} style={{ position: 'absolute', top: 8, right: 8 }} />
}

/** `BODY_CLASS`: `px-4 pt-4 pb-4`, gap per step. */
function Body({ children, gap = 12, align, top }: { children: ReactNode; gap?: number; align?: 'center'; top?: number }) {
  return <div style={{ display: 'flex', flexDirection: 'column', gap, paddingLeft: 16, paddingRight: 16, paddingTop: top ?? 16, paddingBottom: 16, ...(align ? { alignItems: 'center' } : {}) }}>{children}</div>
}
function Title({ children, center, pe }: { children: ReactNode; center?: boolean; pe?: boolean }) {
  const t = useTheme()
  return <Label color={t.text} size={18} lineHeight={22.5} weight={600} style={{ ...(center ? { textAlign: 'center' } : {}), ...(pe ? { paddingRight: 32 } : {}) }}>{children}</Label>
}
function Desc({ children, center, testId }: { children: ReactNode; center?: boolean; testId?: string }) {
  const t = useTheme()
  return <div testId={testId} style={{ display: 'flex', flexDirection: 'column', ...(center ? { alignItems: 'center' } : {}) }}><Label color={t.textSecondary} size={14} lineHeight={19.25} style={center ? { textAlign: 'center' } : {}}>{children}</Label></div>
}
function Quiet({ children }: { children: string }) {
  const t = useTheme()
  return <Label color={t.textSecondary} size={13} lineHeight={19.5}>{children}</Label>
}
function QuietLink({ children, onClick, testId }: { children: string; onClick: () => void; testId?: string }) {
  return <Button testId={testId} variant="ghost" size="sm" onClick={onClick}>{children}</Button>
}
function Checking() {
  const t = useTheme()
  return <div role="status" style={{ display: 'flex', alignItems: 'center', gap: 8 }}><MatrixLoader variant="pulse" dot={2} gap={1.33} /><Label color={t.textSecondary} size={13} lineHeight={19.5}>Checking the sign-in…</Label></div>
}
function RemoteNote({ host, transport }: { host: string; transport: 'SSH' | 'Tailscale' }) {
  const t = useTheme()
  return <div style={{ display: 'flex', flexDirection: 'column', minWidth: 0, paddingLeft: 12, paddingRight: 12, paddingTop: 8, paddingBottom: 8, borderRadius: 8, backgroundColor: t.buttonSecondary }}><Label color={t.textSecondary} size={13} lineHeight={19.5}>{remoteNote(host, transport)}</Label></div>
}
function CopyButton({ text, label }: { text: string; label: string }) {
  const [done, setDone] = useState(false)
  useEffect(() => { if (!done) return; const timer = setTimeout(() => setDone(false), 1400); return () => clearTimeout(timer) }, [done])
  return <Button testId="account-copy" icon={done ? N.check : S.copy} onClick={() => { void copyText(text); setDone(true) }}>{done ? 'Copied' : label}</Button>
}

function ColorSwatches({ value, onChange }: { value: AccountColor; onChange: (color: AccountColor) => void }) {
  const t = useTheme()
  return <div role="radiogroup" aria-label="Color" style={{ display: 'flex', flexWrap: 'wrap', gap: 8 }}>
    {ACCOUNT_COLORS.map((color) => {
      const selected = color === value
      const fill = accountColor(t, color)!
      return <div key={color} testId={`account-color-${color}`} role="radio" aria-checked={selected} aria-label={ACCOUNT_COLOR_LABELS[color]} tabIndex={selected ? 0 : -1} onClick={() => onChange(color)}
        onKeyDown={(e) => {
          const step = e.key === 'right' || e.key === 'down' ? 1 : e.key === 'left' || e.key === 'up' ? -1 : 0
          if (step) onChange(ACCOUNT_COLORS[(ACCOUNT_COLORS.indexOf(color) + step + ACCOUNT_COLORS.length) % ACCOUNT_COLORS.length]!)
          else if (e.key === 'enter' || e.key === 'space') onChange(color)
        }}
        style={{ position: 'relative', display: 'flex', alignItems: 'center', justifyContent: 'center', width: 28, height: 28, borderRadius: 14, cursor: 'pointer' }}>
        <div style={{ width: 16, height: 16, borderRadius: 8, backgroundColor: fill, pointerEvents: 'none' }} />
        <div style={{ position: 'absolute', left: 0, top: 0, width: 28, height: 28, borderRadius: 14, borderWidth: 2, borderColor: selected ? fill : 'transparent', pointerEvents: 'none' }} />
      </div>
    })}
  </div>
}

function PickStep(props: { providers: { kind: ProviderKind; display_name: string; available: boolean }[]; kind: ProviderKind | undefined; onKind: (kind: ProviderKind) => void; folder: string; onFolder: (value: string) => void; advancedOpen: boolean; onAdvancedOpen: (open: boolean) => void; remote: boolean }) {
  const t = useTheme()
  const c = useControls()
  const { providers, kind, remote } = props
  const placeholder = kind === 'claude-code' ? '~/.claude-work' : kind === 'codex' ? '~/.codex-work' : `~/.${kind ?? 'agent'}-work`
  const TILE = Math.floor((448 - 32 - 16) / 3) - 2
  return <>
    {/* `DialogHeader`: px-4 pt-4 pb-2, so the description wraps at the same width as the desktop's. */}
    <div style={{ display: 'flex', flexDirection: 'column', gap: 6, paddingLeft: 16, paddingRight: 16, paddingTop: 16, paddingBottom: 8 }}>
      <Title pe>Add an account</Title>
      <Desc>Sign in with another account. Your current sign-ins stay as they are.</Desc>
    </div>
    <Body gap={16} top={4}>
      <div testId="account-provider" role="radiogroup" aria-label="Agent" style={{ display: 'flex', flexWrap: 'wrap', gap: 8 }}>
        {providers.map((p) => {
          const selected = p.kind === kind
          return <div key={p.kind} testId={`account-provider-${p.kind}`} role="radio" aria-checked={selected} tabIndex={0} onClick={() => props.onKind(p.kind)} onKeyDown={(e) => { if (e.key === 'enter' || e.key === 'space') props.onKind(p.kind) }}
            style={{ display: 'flex', flexDirection: 'column', alignItems: 'flex-start', gap: 8, width: TILE, minWidth: 0, padding: 12, borderWidth: 1, borderColor: selected ? c.accent : t.border, borderRadius: 12, backgroundColor: selected ? c.optionSelected : 'transparent', opacity: p.available ? 1 : 0.55, cursor: 'pointer', hover: { backgroundColor: t.buttonSecondaryHover } }}>
            <ProviderMark kind={p.kind} size={18} />
            <Label color={t.text} size={14} lineHeight={21} weight={500} truncate>{p.display_name}</Label>
            {!p.available && <Label color={t.textSecondary} size={13} lineHeight={19.5} style={{ marginTop: -6 }}>Not installed</Label>}
          </div>
        })}
      </div>
      {kind && kind !== 'opencode' && kind !== 'pi' && <div style={{ display: 'flex', flexDirection: 'column' }}>
        <Button testId="account-advanced" variant="ghost" size="sm" label="Advanced" trailing={<Chevron open={props.advancedOpen} size={12} color={t.textSecondary} />} onClick={() => props.onAdvancedOpen(!props.advancedOpen)} style={{ alignSelf: 'flex-start', marginLeft: -8 }}>Advanced</Button>
        {props.advancedOpen && <div style={{ display: 'flex', flexDirection: 'column', gap: 6, paddingTop: 12, paddingBottom: 4 }}>
          <Label color={t.text} size={14} lineHeight={21} weight={500}>Folder</Label>
          <Label color={ink(t, 0.8)} size={13} lineHeight={19.5} weight={500}>Use an existing folder</Label>
          <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
            <div style={{ flexGrow: 1, display: 'flex', minWidth: 0 }}><TextField testId="account-folder" label="Folder" size="lg" placeholder={placeholder} value={props.folder} onChange={props.onFolder} width="100%" /></div>
            {!remote && <Button testId="account-choose-folder" size="lg" onClick={() => void pickFolder().then((picked) => { if (picked) props.onFolder(picked) })}>Choose…</Button>}
          </div>
          <Label color={t.textSecondary} size={13} lineHeight={19.5}>Kybern uses the sign-in already saved in this folder and never deletes it.</Label>
        </div>}
      </div>}
    </Body>
  </>
}

function PasteStep({ login, remote, host, transport, wrong, code, onCode, shown, onShown, onSubmit, onTerminal }: { login: { url?: string }; remote: boolean; host: string; transport: 'SSH' | 'Tailscale'; wrong: boolean; code: string; onCode: (value: string) => void; shown: boolean; onShown: (value: boolean) => void; onSubmit: () => void; onTerminal: () => void }) {
  const t = useTheme()
  return <Body gap={12}>
    <Title pe>Paste the sign-in code</Title>
    {remote && <RemoteNote host={host} transport={transport} />}
    <div style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
      {['Open the sign-in page and approve access.', 'Copy the code the page shows.', 'Paste it below.'].map((text, index) => <div key={text} style={{ display: 'flex', gap: 8 }}><div style={{ display: 'flex', alignItems: 'center', justifyContent: 'center', width: 18, height: 18, flexShrink: 0, borderRadius: 9, backgroundColor: t.buttonSecondary }}><Label color={t.text} size={12} lineHeight={12} weight={500}>{String(index + 1)}</Label></div><Label color={t.textSecondary} size={14} lineHeight={18}>{text}</Label></div>)}
    </div>
    <div style={{ display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: 8 }}>
      <Button testId="account-open-page" icon={S.externalLink} disabled={!login.url} onClick={() => void openExternal(login.url!)}>Open sign-in page</Button>
      {login.url && <CopyButtonGhost text={login.url} />}
    </div>
    <div style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
      <Label color={t.text} size={14} lineHeight={21} weight={500}>Code</Label>
      <TextField testId="account-code" label="Code" size="lg" secure={!shown} autoFocus={wrong} placeholder="Paste code" value={code} onChange={onCode} onSubmit={onSubmit} invalid={wrong} mono={!!code} width="100%" trailing={<IconButton testId="account-code-toggle" icon={S.eye} label="Show code" size="sm" active={shown} onClick={() => onShown(!shown)} />} />
      {wrong && <Label color={t.v('--destructive')} size={13} lineHeight={19.5}>That code didn’t work. Open the sign-in page again and paste the new code.</Label>}
    </div>
    <QuietLink testId="account-show-terminal" onClick={onTerminal}>Having trouble? Show terminal</QuietLink>
  </Body>
}
function CopyButtonGhost({ text }: { text: string }) {
  const [done, setDone] = useState(false)
  useEffect(() => { if (!done) return; const timer = setTimeout(() => setDone(false), 1400); return () => clearTimeout(timer) }, [done])
  return <Button testId="account-copy-link" variant="ghost" icon={done ? N.check : S.copy} onClick={() => { void copyText(text); setDone(true) }}>{done ? 'Copied' : 'Copy link'}</Button>
}
