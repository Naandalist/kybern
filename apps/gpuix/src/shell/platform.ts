// Bun-side platform seams. Mirrors desktop/src-tauri/src/{remote,cli}.rs.
// All inputs are explicit: tests never inspect ~/.ssh, PATH or installed apps.
// `environments/ssh.ts` executes the SSH plans (bootstrap, tunnel keeper).
import { lstat, mkdir, readlink, realpath, symlink, unlink } from 'node:fs/promises'
import path from 'node:path'
import remoteInstaller from '../../../../scripts/install-remote.sh' with { type: 'text' }

export interface SshTarget { target: string; port?: number }
export interface SshConfig extends SshTarget { remotePort: number; localPort: number; dataDir?: string }
export interface CommandPlan { command: string; args: string[]; stdin?: string; timeoutMs?: number }
export const REMOTE_INSTALL_URL = 'https://github.com/Tresnanda/kybern/releases/latest/download/kybern-remote-install.sh'
export const UPDATE_MANIFEST_URL = 'https://github.com/Tresnanda/kybern/releases/latest/download/latest.json'

export function parseSshTarget(input: string): SshTarget {
  let target = input.trim().replace(/^ssh:\/\//, '').replace(/\/+$/, '')
  if (!target) throw Error('Enter the machine as user@host')
  let port: number | undefined
  const match = /^(.*):(\d+)$/.exec(target)
  if (match) {
    target = match[1]!
    port = Number(match[2])
    if (!Number.isInteger(port) || port < 1 || port > 65535) throw Error('Enter an SSH port between 1 and 65535')
  }
  if (!target || !target.split('@').every((part) => /^[a-z\d_.][a-z\d_.-]*$/i.test(part)) || target.split('@').length > 2) throw Error('Enter the machine as user@host, using only letters, digits, dots, dashes and underscores')
  return { target, ...(port ? { port } : {}) }
}

export function sshConfigHosts(text: string): string[] {
  const hosts = new Set<string>()
  for (const line of text.split('\n')) {
    const match = /^\s*Host\s+(.+)$/.exec(line)
    if (!match) continue
    for (const host of match[1]!.split('#')[0]!.trim().split(/\s+/)) {
      try { hosts.add(parseSshTarget(host).target) } catch { /* Wildcard and negated patterns are not choices. */ }
    }
  }
  return [...hosts]
}

export const shellQuote = (value: string) => `'${value.replaceAll("'", "'\\''")}'`
const portNumber = (value: number) => { if (!Number.isInteger(value) || value < 1 || value > 65535) throw Error('Choose a port between 1 and 65535'); return value }
export function sshArguments(config: SshTarget): string[] {
  const parsed = parseSshTarget(config.target)
  if (parsed.port) throw Error('Pass the SSH port separately')
  return ['-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=accept-new', '-o', 'ConnectTimeout=15', '-o', 'ServerAliveInterval=15', '-o', 'ServerAliveCountMax=3', '-o', 'LogLevel=ERROR', ...(config.port !== undefined ? ['-p', String(portNumber(config.port))] : [])]
}
export function remoteScript(config: SshConfig, script: string, timeoutMs: number): CommandPlan {
  const data = config.dataDir ? shellQuote(config.dataDir) : '"$HOME/.kybern"'
  return { command: 'ssh', args: [...sshArguments(config), config.target, 'sh'], timeoutMs,
    stdin: `PATH="$HOME/.cargo/bin:$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"; export PATH\nKYBERN_DATA_DIR=${data}; export KYBERN_DATA_DIR\n${script}\n` }
}
export function tunnelPlan(config: SshConfig): CommandPlan {
  return { command: 'ssh', args: [...sshArguments(config), '-o', 'ExitOnForwardFailure=yes', '-o', 'ControlMaster=no', '-o', 'ControlPath=none', '-N', '-L', `127.0.0.1:${portNumber(config.localPort)}:127.0.0.1:${portNumber(config.remotePort)}`, config.target] }
}

/** Trusted network setup embeds the verified installer and pins it to the desktop release.
 * Automatic probes require a previously verified SSH host key; no first-contact trust prompt. */
export function networkBootstrapPlan(config: SshConfig, version: string) {
  if (!/^\d+\.\d+\.\d+$/.test(version)) throw Error('Use a published Kybern version for remote setup.')
  const strict = (plan: CommandPlan): CommandPlan => ({ ...plan, args: plan.args.map(arg => arg === 'StrictHostKeyChecking=accept-new' ? 'StrictHostKeyChecking=yes' : arg) })
  return {
    probe: strict(remoteScript(config, PROBE_SCRIPT, 45000)),
    installIfMissingOrOlder: strict(remoteScript(config, `set -- --version ${shellQuote(version)}\n${remoteInstaller}`, 600000)),
    startIfUnreachable: strict(remoteScript(config, `set -- --service-only --port ${portNumber(config.remotePort)}\n${remoteInstaller}`, 60000)),
  }
}

export function bootstrapPlan(config: SshConfig, installer = REMOTE_INSTALL_URL) {
  const url = new URL(installer)
  if (url.protocol !== 'https:') throw Error('Use an HTTPS installer address')
  return {
    probe: remoteScript(config, PROBE_SCRIPT, 45000),
    installIfMissingOrOlder: remoteScript(config, `set -eu\ncommand -v curl >/dev/null 2>&1 || { echo 'curl is not installed on this machine; install it and try again' >&2; exit 1; }\ncurl --proto '=https' --tlsv1.2 -LsSf ${shellQuote(url.href)} | sh`, 600000),
    startIfUnreachable: remoteScript(config, START_SCRIPT.replaceAll('{port}', String(portNumber(config.remotePort))), 60000),
    tunnel: tunnelPlan(config),
  }
}

/** Read-only manifest check. Installing a signed GPUIX artifact is not available yet. */
export async function checkRelease(currentVersion: string, fetcher: (url: string, init: RequestInit) => Promise<Response> = fetch) {
  const response = await fetcher(UPDATE_MANIFEST_URL, { signal: AbortSignal.timeout(15000) })
  if (!response.ok) throw Error(`Unable to check for updates (${response.status}). Try again.`)
  const data: unknown = await response.json()
  if (!data || typeof data !== 'object' || !('version' in data) || typeof data.version !== 'string') throw Error('The update manifest is incomplete. Try again later.')
  const newer = compareVersions(data.version, currentVersion) > 0
  return { version: data.version, newer, notes: 'notes' in data && typeof data.notes === 'string' ? data.notes : '', installable: false as const }
}
function versionParts(value: string) {
  const match = /^v?(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+.*)?$/.exec(value.trim())
  if (!match) throw Error('The release version is invalid')
  return { core: match.slice(1, 4).map(Number), pre: match[4]?.split('.') ?? [] }
}
/**
 * Semver precedence: `0.21.0-dev.2` sorts after `0.20.x` and before `0.21.0`, and `dev.10` after `dev.9`
 * (numeric identifiers compare as numbers). Build metadata (`+…`) is ignored.
 */
export function compareVersions(a: string, b: string) {
  const left = versionParts(a), right = versionParts(b)
  for (let i = 0; i < 3; i++) if (left.core[i] !== right.core[i]) return left.core[i]! - right.core[i]!
  // A release is newer than any of its prereleases.
  if (!left.pre.length || !right.pre.length) return right.pre.length - left.pre.length
  for (let i = 0; i < Math.min(left.pre.length, right.pre.length); i++) {
    const x = left.pre[i]!, y = right.pre[i]!
    if (x === y) continue
    const numeric = /^\d+$/
    if (numeric.test(x) && numeric.test(y)) return Number(x) - Number(y)
    if (numeric.test(x) !== numeric.test(y)) return numeric.test(x) ? -1 : 1
    return x < y ? -1 : 1
  }
  return left.pre.length - right.pre.length
}

export interface CliPaths { bundled: string; target: string }
/** macOS/Linux symlink install. No admin escalation, PATH edit, or implicit path. */
export async function cliStatus({ bundled, target }: CliPaths) {
  if (!path.isAbsolute(bundled) || !path.isAbsolute(target)) throw Error('Choose absolute paths for the bundled command and installation target')
  const source = await realpath(bundled)
  let installed = false, occupied = false
  try { occupied = true; installed = await realpath(target) === source }
  catch (e) { if ((e as NodeJS.ErrnoException).code !== 'ENOENT') throw e; try { await lstat(target); occupied = true } catch (missing) { if ((missing as NodeJS.ErrnoException).code !== 'ENOENT') throw missing; occupied = false } }
  return { bundled: source, target, installed, occupied }
}
export async function installCli(paths: CliPaths) {
  const status = await cliStatus(paths)
  if (status.installed) return status
  if (status.occupied) throw Error('A different command already exists at this path. Choose another location.')
  await mkdir(path.dirname(paths.target), { recursive: true })
  // symlink is exclusive: a concurrent creator cannot be overwritten.
  await symlink(status.bundled, paths.target)
  return cliStatus(paths)
}
export async function uninstallCli(paths: CliPaths) {
  if (!path.isAbsolute(paths.bundled) || !path.isAbsolute(paths.target)) throw Error('Choose absolute paths for the bundled command and installation target')
  const stat = await lstat(paths.target).catch((error: NodeJS.ErrnoException) => { if (error.code === 'ENOENT') return null; throw error })
  if (!stat) return
  if (!stat.isSymbolicLink()) throw Error('This command is not a link installed by Kybern')
  const linked = path.resolve(path.dirname(paths.target), await readlink(paths.target))
  if (linked !== await realpath(paths.bundled)) throw Error('This command belongs to another installation')
  await unlink(paths.target)
}

// Kept in step with desktop/src-tauri/src/remote.rs. Scripts are plan data only.
const PROBE_SCRIPT = String.raw`
set -u
printf 'os=%s\n' "$(uname -s)"
printf 'arch=%s\n' "$(uname -m)"
if command -v kybernd >/dev/null 2>&1; then
  printf 'kybernd=%s\n' "$(command -v kybernd)"
  printf 'version=%s\n' "$(kybernd --version 2>/dev/null | awk '{print $NF}')"
fi
command -v curl >/dev/null 2>&1 && echo curl=1
[ -f "$KYBERN_DATA_DIR/daemon.port" ] && printf 'port=%s\n' "$(cat "$KYBERN_DATA_DIR/daemon.port")"
[ -f "$KYBERN_DATA_DIR/daemon.token" ] && printf 'token=%s\n' "$(cat "$KYBERN_DATA_DIR/daemon.token")"
exit 0
`

const START_SCRIPT = String.raw`set -eu
mkdir -p "$KYBERN_DATA_DIR"
if command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1; then
  unit="$HOME/.config/systemd/user/kybernd.service"
  if [ ! -f "$unit" ]; then
    mkdir -p "$(dirname "$unit")"
    cat > "$unit" <<UNIT
[Unit]
Description=Kybern agent daemon

[Service]
ExecStart=$(command -v kybernd) --bind 127.0.0.1 --port {port} --data-dir $KYBERN_DATA_DIR
WorkingDirectory=%h
Environment="PATH=$PATH"
Restart=on-failure
RestartSec=5
KillSignal=SIGINT
TimeoutStopSec=30
OOMPolicy=continue

[Install]
WantedBy=default.target
UNIT
  fi
  # Units written before OOMPolicy=continue stop the daemon and every agent when one agent runs out of memory.
  if ! grep -Eq '^OOMPolicy=' "$unit" && ! grep -Eqs '^OOMPolicy=' "$unit.d"/*.conf; then
    mkdir -p "$unit.d"
    printf '[Service]\nOOMPolicy=continue\n' > "$unit.d/oom.conf"
  fi
  systemctl --user daemon-reload
  systemctl --user enable --now kybernd >/dev/null 2>&1 || systemctl --user restart kybernd
  command -v loginctl >/dev/null 2>&1 && loginctl enable-linger "$(id -un)" >/dev/null 2>&1 || true
else
  nohup kybernd --bind 127.0.0.1 --port {port} --data-dir "$KYBERN_DATA_DIR" >>"$KYBERN_DATA_DIR/daemon.log" 2>&1 </dev/null &
fi
`
