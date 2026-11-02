// Execute the actual embedded installer with an isolated home and process boundary. Nothing
// reaches the network or the real service manager; archive parsing and checksum verification are real.
import { describe, expect, test } from 'bun:test'
import { createHash } from 'node:crypto'
import { mkdtemp, mkdir, readFile, readdir, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import path from 'node:path'

const script = path.resolve(import.meta.dir, '../../../../scripts/install-remote.sh')
async function fixture(os: 'Linux' | 'Darwin' | 'Windows_NT' = 'Linux', badChecksum = false) {
  const root = await mkdtemp(path.join(tmpdir(), 'kybern-installer-test-'))
  const home = path.join(root, 'home'), mock = path.join(root, 'mock'), payload = path.join(root, 'payload'), archive = path.join(root, 'release.tar.xz'), log = path.join(root, 'calls')
  await Promise.all([mkdir(home), mkdir(mock), mkdir(payload)])
  for (const name of ['kybern', 'kybernd']) await writeFile(path.join(payload, name), '#!/bin/sh\necho fixture\n', { mode: 0o755 })
  const tar = Bun.spawn(['tar', '-cJf', archive, '-C', payload, '.'], { stdout: 'pipe', stderr: 'pipe' })
  expect(await tar.exited).toBe(0)
  const checksum = badChecksum ? '0'.repeat(64) : createHash('sha256').update(await readFile(archive)).digest('hex')
  await writeFile(path.join(root, 'checksum'), `${checksum}  kybern.tar.xz\n`)
  const mocks: Record<string, string> = {
    uname: `case "$1" in -s) echo ${os};; -m) echo x86_64;; esac`,
    curl: `printf '%s\\n' "$*" >> "$TEST_LOG"\ncase "$*" in *.sha256*) cat "$TEST_CHECKSUM";; *) while [ "$1" != -o ]; do shift; done; cp "$TEST_ARCHIVE" "$2";; esac`,
    systemctl: 'printf "systemctl %s\\n" "$*" >> "$TEST_LOG"\nexit 0',
    loginctl: 'echo yes',
    launchctl: 'printf "launchctl %s\\n" "$*" >> "$TEST_LOG"\ncase "$*" in "print gui/"*/dev.kybern.daemon) exit 1;; esac\nexit 0',
  }
  for (const [name, body] of Object.entries(mocks)) await writeFile(path.join(mock, name), `#!/bin/sh\n${body}\n`, { mode: 0o755 })
  // HOME is scoped to a child fixture, never changed in the agent or user's shell.
  const env = { ...process.env, HOME: home, PATH: `${mock}:/usr/bin:/bin`, TMPDIR: root, KYBERN_INSTALL_DIR: path.join(home, '.cargo/bin'), KYBERN_DATA_DIR: path.join(home, '.kybern'), TEST_ARCHIVE: archive, TEST_CHECKSUM: path.join(root, 'checksum'), TEST_LOG: log }
  const run = async (args: string[] = [], extra: Record<string, string> = {}) => {
    const child = Bun.spawn(['sh', script, '--version', '9.8.7', ...args], { env: { ...env, ...extra }, stdout: 'pipe', stderr: 'pipe' })
    const [code, stdout, stderr] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()])
    return { code, stdout, stderr }
  }
  return { root, home, mock, env, run, calls: () => readFile(log, 'utf8'), close: () => rm(root, { recursive: true, force: true }) }
}
describe('verified persistent remote installer', () => {
  test('uses one pinned archive and checksum, installs both commands, enables a Linux user service', async () => {
    const f = await fixture()
    try {
      const result = await f.run(['--service'])
      expect(result.code).toBe(0)
      expect(await readFile(path.join(f.home, '.cargo/bin/kybern'), 'utf8')).toContain('fixture')
      expect(await readFile(path.join(f.home, '.cargo/bin/kybernd'), 'utf8')).toContain('fixture')
      const unit = await readFile(path.join(f.home, '.config/systemd/user/kybernd.service'), 'utf8')
      expect(unit).toContain(`--data-dir ${f.home}/.kybern`)
      expect(unit).toContain('Restart=on-failure')
      // One agent running out of memory must not stop the daemon and every other agent.
      expect(unit).toContain('\nOOMPolicy=continue\n')
      const calls = await f.calls()
      expect(calls).toContain('/releases/download/v9.8.7/kybern-x86_64-unknown-linux-musl.tar.xz')
      expect(calls).toContain('.tar.xz.sha256')
      expect(calls).toContain('systemctl --user enable --now kybernd')
      expect((await readdir(f.root)).some(name => name.startsWith('kybern-install.'))).toBe(false)
    } finally { await f.close() }
  })
  test('checksum mismatch leaves installed commands and services untouched', async () => {
    const f = await fixture('Linux', true)
    try {
      const result = await f.run(['--service'])
      expect(result.code).not.toBe(0)
      expect(result.stderr).toContain('does not match its published checksum')
      expect(await Bun.file(path.join(f.home, '.cargo/bin/kybernd')).exists()).toBe(false)
      expect((await f.calls()).includes('systemctl')).toBe(false)
      expect((await readdir(f.root)).some(name => name.startsWith('kybern-install.'))).toBe(false)
    } finally { await f.close() }
  })
  test('macOS writes a persistent LaunchAgent and bootstraps the logged-in user domain', async () => {
    const f = await fixture('Darwin')
    try {
      expect((await f.run(['--service'])).code).toBe(0)
      const plist = await readFile(path.join(f.home, 'Library/LaunchAgents/dev.kybern.daemon.plist'), 'utf8')
      expect(plist).toContain('<key>RunAtLoad</key><true/><key>KeepAlive</key><true/>')
      expect(plist).toContain(`<string>${f.home}/.kybern</string>`)
      expect(await f.calls()).toContain('launchctl bootstrap gui/')
    } finally { await f.close() }
  })
  test('an existing Kybern unit without an OOM policy gets the drop-in and is not rewritten', async () => {
    const f = await fixture()
    try {
      const unit = path.join(f.home, '.config/systemd/user/kybernd.service')
      const old = `[Unit]\nDescription=Kybern agent daemon\n[Service]\nExecStart=${f.home}/.cargo/bin/kybernd --bind 127.0.0.1 --port 4173 --data-dir ${f.home}/.kybern\nRestart=on-failure\n`
      await mkdir(path.dirname(unit), { recursive: true }); await writeFile(unit, old)
      expect((await f.run(['--service'])).code).toBe(0)
      expect(await readFile(unit, 'utf8')).toBe(old)
      expect(await readFile(`${unit}.d/oom.conf`, 'utf8')).toBe('[Service]\nOOMPolicy=continue\n')
      expect(await f.calls()).toContain('systemctl --user daemon-reload')
    } finally { await f.close() }
  })
  test('does not claim persistence without lingering and does not overwrite a different service', async () => {
    const f = await fixture()
    try {
      await writeFile(path.join(f.mock, 'loginctl'), '#!/bin/sh\nexit 1\n', { mode: 0o755 })
      const failed = await f.run(['--service'])
      expect(failed.code).not.toBe(0)
      expect(failed.stderr).toContain('administrator')
      expect(await Bun.file(path.join(f.home, '.config/systemd/user/kybernd.service')).exists()).toBe(false)
      await writeFile(path.join(f.mock, 'loginctl'), '#!/bin/sh\necho yes\n', { mode: 0o755 })
      const unit = path.join(f.home, '.config/systemd/user/kybernd.service')
      await mkdir(path.dirname(unit), { recursive: true }); await writeFile(unit, 'UNRELATED SERVICE')
      const conflict = await f.run(['--service'])
      expect(conflict.code).not.toBe(0)
      expect(conflict.stderr).toContain('another configuration')
      expect(await readFile(unit, 'utf8')).toBe('UNRELATED SERVICE')
    } finally { await f.close() }
  })
  test('Windows and service-directive injection fail before any download or service call', async () => {
    const f = await fixture('Windows_NT'), linux = await fixture()
    try {
      const unsupported = await f.run(['--service'])
      expect(unsupported.code).not.toBe(0)
      expect(unsupported.stderr).toContain('kybern-installer.ps1')
      const invalid = await linux.run(['--service'], { KYBERN_DATA_DIR: '/tmp/data\nExecStart=evil' })
      expect(invalid.code).not.toBe(0)
      expect(invalid.stderr).toContain('Service setup needs paths')
      expect(await Bun.file(path.join(linux.root, 'calls')).exists()).toBe(false)
    } finally { await f.close(); await linux.close() }
  })
})
