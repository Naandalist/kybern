import { describe, expect, test } from 'bun:test'
import { RpcCallError } from '../../../../packages/kybern-client/src/client.ts'
import { ghProblem } from '../../../../packages/kybern-client/src/prReview.ts'
import { GH_SIGN_IN, ghInstallCommand } from './ghSetup.ts'
import { problemCopy } from './problemCopy.ts'

const daemonError = (message: string, problem: object) => new RpcCallError({ code: -32003, message, data: { problem } })

describe('problem copy', () => {
  test('a missing gh says why, offers the install and never shows the raw error', () => {
    const copy = problemCopy(ghProblem(daemonError('The GitHub CLI (gh) isn’t installed.', { kind: 'tool_missing', subject: 'gh', detail: 'run gh: program not found' })), 'pull requests')
    expect(copy.title).toBe('Install GitHub CLI to see pull requests')
    expect(copy.body).toContain('your own GitHub sign-in')
    expect([copy.action, copy.primaryLabel, copy.retryLabel]).toEqual(['install-gh', 'Install GitHub CLI', 'Check again'])
    expect(`${copy.title} ${copy.body}`).not.toContain('RpcCallError')
  })

  test('the old daemon’s bare "run gh" reads the same way', () => {
    expect(problemCopy(ghProblem(new RpcCallError({ code: -32602, message: 'run gh' })), 'pull requests').action).toBe('install-gh')
  })

  test('signed out offers the sign-in', () => {
    const copy = problemCopy(ghProblem(daemonError('gh isn’t signed in.', { kind: 'signed_out', subject: 'gh' })), 'workflow runs')
    expect([copy.title, copy.action, copy.primaryLabel]).toEqual(['Sign in to GitHub to see workflow runs', 'sign-in-gh', 'Sign in to GitHub'])
  })

  test('other cases say what to do next and keep the detail for the disclosure', () => {
    const forbidden = ghProblem(daemonError('GitHub refused this', { kind: 'permission', subject: 'gh', detail: 'HTTP 403: Resource not accessible' }))
    expect(problemCopy(forbidden, 'this pull request')).toMatchObject({ title: 'GitHub refused access', action: null, retryLabel: 'Try again' })
    expect(forbidden.detail).toBe('HTTP 403: Resource not accessible')
    expect(problemCopy({ kind: 'not_found', subject: 'github_remote' }, 'pull requests').title).toBe('This project isn’t on GitHub')
    const other = ghProblem(new Error('gh pr list: boom'))
    expect(problemCopy(other, 'pull requests')).toMatchObject({ title: 'Unable to load pull requests', action: null })
    expect(other.detail).toBe('gh pr list: boom')
    // A kind this client has no copy for uses the daemon's fallback words.
    expect(problemCopy({ kind: 'offline', title: 'Your Mac is asleep', hint: 'Wake it, then try again.' }, 'pull requests')).toMatchObject({ title: 'Your Mac is asleep', body: 'Wake it, then try again.' })
  })
})

describe('gh setup commands', () => {
  test('winget on Windows, Homebrew only where it exists, else the install guide', () => {
    expect(ghInstallCommand('win32')).toBe('winget install --id GitHub.cli -e')
    expect(ghInstallCommand('darwin', p => p === '/opt/homebrew/bin/brew')).toBe('brew install gh')
    expect(ghInstallCommand('darwin', () => false)).toBeNull()
    expect(ghInstallCommand('linux', () => true)).toBeNull()
    expect(GH_SIGN_IN.startsWith('gh auth login -h github.com --web')).toBe(true)
  })
})
