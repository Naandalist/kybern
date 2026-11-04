// Quarantine reporting for CI's test jobs (.github/workflows/ci.yml).
//
// nextest's `ci` profile retries a failed test once. A test that then passes
// does not fail the job; it is "flaky" and appears in the JUnit report as a
// passing test case with <flakyFailure> attempts. This script lists those tests
// in the job summary and as a warning annotation. It files no GitHub issues by
// default: the maintainer tracks flakes outside the issue tracker. With
// FLAKY_FILE_ISSUES=1 on a push to main it files one issue per test (labelled
// `flaky`, titled after the test, deduplicated by title against open and closed
// issues), or comments on the existing one and reopens it; at most MAX_ISSUES a
// run, so a broken runner cannot flood the tracker. A test that failed every
// attempt is a failure, not a flake, and never gets an issue. Reporting never fails the job:
// a missing report (the build failed) or an API error only leaves a warning.
import { execFileSync } from 'node:child_process'
import { appendFileSync, existsSync, readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

export const LABEL = 'flaky'
export const MAX_ISSUES = 5

/** Issues are filed only when FLAKY_FILE_ISSUES=1 on a push to main (CI does not set it); otherwise the summary alone. */
export const filesIssues = env => env.FLAKY_FILE_ISSUES === '1' && env.GITHUB_EVENT_NAME === 'push' && env.GITHUB_REF === 'refs/heads/main'
const EXCERPT_LINES = 40

const decode = text => text.replace(/&(#x[0-9a-f]+|#\d+|amp|lt|gt|quot|apos);/gi, (_, entity) => {
  const named = { amp: '&', lt: '<', gt: '>', quot: '"', apos: "'" }[entity.toLowerCase()]
  if (named) return named
  const code = entity[1] === 'x' || entity[1] === 'X' ? parseInt(entity.slice(2), 16) : parseInt(entity.slice(1), 10)
  return String.fromCodePoint(code)
})

const attribute = (tag, name) => {
  const value = tag.match(new RegExp(`\\s${name}="([^"]*)"`))?.[1]
  return value === undefined ? undefined : decode(value)
}

/** The run's totals and its flaky tests: `{ tests, failures, flaky: [{ suite, name, attempts, output }] }`. */
export function parseJUnit(xml) {
  const root = xml.match(/<testsuites\b[^>]*>/)?.[0] ?? ''
  const flaky = []
  for (const [, open, body = ''] of xml.matchAll(/(<testcase\b[^>]*?)(?:\/>|>([\s\S]*?)<\/testcase>)/g)) {
    const attempts = [...body.matchAll(/<flaky(?:Failure|Error)\b[^>]*?(?:\/>|>([^<]*))/g)]
    if (!attempts.length) continue
    const output = decode(attempts[0][1] ?? '').trim()
    flaky.push({ suite: attribute(open, 'classname') ?? '', name: attribute(open, 'name') ?? '', attempts: attempts.length, output })
  }
  return { tests: Number(attribute(root, 'tests') ?? 0), failures: Number(attribute(root, 'failures') ?? 0), flaky }
}

// A flaky issue filed by hand ("Flaky: orchestrator::tests::x") names the test as a whole word.
const namesTest = (title, name) => name && new RegExp(`(^|[\\s\`'"(])${name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}($|[\\s\`'")])`).test(title)

/** The issue title: the nextest binary id and the test's path, e.g. `Flaky test: kybern-daemon network::tests::x`. */
export const issueTitle = test => `Flaky test: ${test.suite} ${test.name}`.trim()

const excerpt = output => {
  const lines = output.split('\n')
  const shown = lines.slice(0, EXCERPT_LINES).join('\n').replaceAll('```', "'''")
  return `\`\`\`\n${shown}${lines.length > EXCERPT_LINES ? '\n…' : ''}\n\`\`\``
}

/** One line of context for the run: where and on what it flaked. */
export const runContext = env => {
  const url = env.GITHUB_SERVER_URL && env.GITHUB_REPOSITORY && env.GITHUB_RUN_ID
    ? `${env.GITHUB_SERVER_URL}/${env.GITHUB_REPOSITORY}/actions/runs/${env.GITHUB_RUN_ID}` : 'a local run'
  const where = [env.FLAKY_RUNNER, env.GITHUB_EVENT_NAME, env.GITHUB_REF, env.GITHUB_SHA?.slice(0, 9)].filter(Boolean).join(', ')
  return where ? `${url} (${where})` : url
}

export function issueBody(test, context) {
  return [
    `\`${test.suite} ${test.name}\` failed and then passed when nextest retried it, so CI quarantined it instead of failing.`,
    '',
    `First seen: ${context}`,
    '',
    'Quarantine keeps CI green, but the test still has to be fixed or removed (AGENTS.md, "Tests and checks"): wait on an event or condition, not a sleep or a clock. Each new flake adds a comment here.',
    '',
    'Failed attempt:',
    '',
    excerpt(test.output),
  ].join('\n')
}

export const commentBody = (test, context) => [`Flaked again: ${context}`, '', excerpt(test.output)].join('\n')

export function summary(report, results, filing = true) {
  if (!report) return '### Flaky tests\n\nNo nextest report was written, so flaky tests could not be checked (did the build fail?).\n'
  if (!report.flaky.length) return `### Flaky tests\n\nNone: ${report.tests} tests ran without needing a retry.\n`
  const count = report.flaky.length
  const rows = report.flaky.map((test, index) => {
    const result = results[index]
    const issue = !filing ? 'not filed' : result?.url ? `[#${result.number}](${result.url}) ${result.action}`
      : result?.skipped ? 'not filed (over the limit)' : `not filed: ${result?.error ?? 'unknown error'}`
    return `| \`${test.suite}\` \`${test.name}\` | ${test.attempts} | ${issue} |`
  })
  const skipped = results.filter(result => result?.skipped).length
  return [
    '### Flaky tests',
    '',
    `${count} test${count === 1 ? '' : 's'} failed and then passed on retry. They are quarantined: the job does not fail, but each one has to be fixed.`,
    ...(filing ? [] : ['', 'Flaky tests are listed here and in the run annotations; no issues are filed.']),
    ...(skipped ? ['', `${skipped} more flaky test${skipped === 1 ? ' was' : 's were'} not filed: a run files at most ${MAX_ISSUES} issues. So many flakes at once usually means a broken runner or a shared cause; look at the run first.`] : []),
    '',
    '| Test | Failed attempts | Issue |',
    '| --- | --- | --- |',
    ...rows,
    '',
  ].join('\n')
}

/** GitHub calls through `gh api`, which reads GH_TOKEN. */
export function ghClient(repository, run = (args, input) => execFileSync('gh', args, {
  encoding: 'utf8', input, timeout: 60_000, maxBuffer: 32 * 1024 * 1024, stdio: ['pipe', 'pipe', 'pipe'],
})) {
  if (!/^[\w.-]+\/[\w.-]+$/.test(repository ?? '')) throw new Error('Invalid repository')
  const api = (method, endpoint, body) => {
    const args = ['api', '--method', method, endpoint]
    if (body) args.push('--input', '-')
    const text = run(args, body ? JSON.stringify(body) : undefined)
    return text.trim() ? JSON.parse(text) : null
  }
  return {
    // Every issue with the label, open or closed; pull requests are not issues here.
    labelled: () => JSON.parse(run(['api', '--paginate', '--slurp', `repos/${repository}/issues?labels=${LABEL}&state=all&per_page=100`]) || '[]')
      .flat().filter(issue => !issue.pull_request),
    ensureLabel: () => {
      try {
        api('POST', `repos/${repository}/labels`, { name: LABEL, color: 'fbca04', description: 'A test that failed and then passed on retry in CI' })
      } catch (error) {
        // 422: the label exists already.
        if (!/422|already_exists/.test(String(error.stderr ?? error.message))) throw error
      }
    },
    create: (title, body) => api('POST', `repos/${repository}/issues`, { title, body, labels: [LABEL] }),
    reopen: number => api('PATCH', `repos/${repository}/issues/${number}`, { state: 'open' }),
    comment: (number, body) => api('POST', `repos/${repository}/issues/${number}/comments`, { body }),
  }
}

/**
 * File or update one issue per flaky test, for at most `limit` distinct tests.
 * Returns `{ action, number, url }`, `{ skipped: true }` (over the limit) or `{ error }` per test.
 */
export function fileIssues(flaky, client, context, limit = MAX_ISSUES) {
  if (!flaky.length) return []
  let existing
  try {
    client.ensureLabel()
    existing = client.labelled()
  } catch (error) {
    const message = `could not read the flaky issues (${firstLine(error)})`
    return flaky.map(() => ({ error: message }))
  }
  const results = []
  const handled = new Set()
  for (const test of flaky) {
    const title = issueTitle(test)
    if (!handled.has(title) && handled.size >= limit) {
      results.push({ skipped: true })
      continue
    }
    handled.add(title)
    try {
      const issue = existing.find(issue => issue.title === title) ?? existing.find(issue => namesTest(issue.title, test.name))
      if (!issue) {
        const created = client.create(title, issueBody(test, context))
        existing.push(created)
        results.push({ action: 'filed', number: created.number, url: created.html_url })
        continue
      }
      if (issue.state === 'closed') client.reopen(issue.number)
      client.comment(issue.number, commentBody(test, context))
      results.push({ action: issue.state === 'closed' ? 'reopened' : 'updated', number: issue.number, url: issue.html_url })
    } catch (error) {
      results.push({ error: firstLine(error) })
    }
  }
  return results
}

const firstLine = error => String(error.stderr || error.message || error).trim().split('\n')[0]

function main() {
  const path = process.argv[2] ?? 'target/nextest/ci/junit.xml'
  const report = existsSync(path) ? parseJUnit(readFileSync(path, 'utf8')) : null
  let results = []
  const filing = filesIssues(process.env)
  if (report?.flaky.length) {
    for (const test of report.flaky) console.log(`::warning title=Flaky test (quarantined)::${test.suite} ${test.name} failed and then passed on retry`)
  }
  if (report?.flaky.length && filing) {
    try {
      results = fileIssues(report.flaky, ghClient(process.env.GITHUB_REPOSITORY), runContext(process.env))
    } catch (error) {
      results = report.flaky.map(() => ({ error: firstLine(error) }))
    }
    for (const result of results) {
      if (result.error) console.log(`::warning title=Flaky test issue not filed::${result.error}`)
      else if (result.url) console.log(`${result.action} ${result.url}`)
    }
  }
  const text = summary(report, results, filing)
  console.log(text)
  if (process.env.GITHUB_STEP_SUMMARY) appendFileSync(process.env.GITHUB_STEP_SUMMARY, text)
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try { main() } catch (error) { console.log(`::warning title=Flaky test report failed::${firstLine(error)}`) }
}
