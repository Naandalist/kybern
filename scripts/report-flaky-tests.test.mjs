import { test } from 'node:test'
import assert from 'node:assert/strict'
import { MAX_ISSUES, commentBody, fileIssues, filesIssues, ghClient, issueBody, issueTitle, parseJUnit, runContext, summary } from './report-flaky-tests.mjs'

// The shape cargo-nextest writes (https://nexte.st/docs/machine-readable/junit).
const junit = `<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="nextest-run" tests="4" failures="1" errors="0" uuid="u" timestamp="t" time="1.2">
    <testsuite name="kybern-daemon" tests="3" disabled="0" errors="0" failures="0">
        <testcase name="plain::passes" classname="kybern-daemon" timestamp="t" time="0.004">
        </testcase>
        <testcase name="empty::self_closing" classname="kybern-daemon" timestamp="t" time="0.001"/>
        <testcase name="network::ports::tests::flaky_&lt;port&gt;" classname="kybern-daemon" timestamp="t" time="0.004">
            <flakyFailure timestamp="t" time="0.004" type="test failure">thread &apos;network::ports::tests::flaky&apos; panicked at crates/kybern-daemon/src/network/ports.rs:567:9:
assertion \`left == right\` failed
  left: 1
 right: 2
                <system-out>running 1 test</system-out>
                <system-err>thread &apos;x&apos; panicked</system-err>
            </flakyFailure>
        </testcase>
    </testsuite>
    <testsuite name="kybern-drivers::live_drivers" tests="1" disabled="0" errors="0" failures="1">
        <testcase name="fails_twice" classname="kybern-drivers::live_drivers" timestamp="t" time="0.004">
            <failure type="test failure">boom</failure>
            <rerunFailure timestamp="t" time="0.004" type="test failure">boom</rerunFailure>
        </testcase>
        <testcase name="error_then_pass" classname="kybern-drivers::live_drivers" timestamp="t" time="0.004">
            <flakyError timestamp="t" time="0.004" type="test abort"/>
            <flakyError timestamp="t" time="0.004" type="test abort">signal 9</flakyError>
        </testcase>
    </testsuite>
</testsuites>`

test('only tests that passed after failing are flaky; a test that failed every attempt is not', () => {
  const report = parseJUnit(junit)
  assert.equal(report.tests, 4)
  assert.equal(report.failures, 1)
  assert.deepEqual(report.flaky.map(t => [t.suite, t.name, t.attempts]), [
    ['kybern-daemon', 'network::ports::tests::flaky_<port>', 1],
    ['kybern-drivers::live_drivers', 'error_then_pass', 2],
  ])
  assert.match(report.flaky[0].output, /^thread 'network::ports::tests::flaky' panicked at crates\/kybern-daemon\/src\/network\/ports\.rs:567:9:\nassertion `left == right` failed/)
  assert.doesNotMatch(report.flaky[0].output, /running 1 test/)
  assert.equal(parseJUnit('<testsuites tests="0"></testsuites>').flaky.length, 0)
})

test('the issue is titled after the test and the body says it still needs a fix', () => {
  const [flaky] = parseJUnit(junit).flaky
  assert.equal(issueTitle(flaky), 'Flaky test: kybern-daemon network::ports::tests::flaky_<port>')
  const context = runContext({ GITHUB_SERVER_URL: 'https://github.com', GITHUB_REPOSITORY: 'o/r', GITHUB_RUN_ID: '9', FLAKY_RUNNER: 'macos-15', GITHUB_EVENT_NAME: 'push', GITHUB_REF: 'refs/heads/main', GITHUB_SHA: 'abcdef0123456789' })
  assert.equal(context, 'https://github.com/o/r/actions/runs/9 (macos-15, push, refs/heads/main, abcdef012)')
  const body = issueBody(flaky, context)
  assert.match(body, /First seen: https:\/\/github\.com\/o\/r\/actions\/runs\/9/)
  assert.match(body, /still has to be fixed/)
  assert.match(body, /ports\.rs:567:9/)
  assert.match(commentBody(flaky, context), /^Flaked again: https:/)
  // A fence inside the output cannot end the excerpt early.
  assert.doesNotMatch(issueBody({ ...flaky, output: 'a\n```\nb' }, context).split('Failed attempt:')[1].slice(5, -4), /```/)
})

function fakeClient(issues) {
  const calls = []
  let next = 100
  return {
    calls,
    ensureLabel: () => calls.push(['label']),
    labelled: () => issues,
    create: (title, body) => { calls.push(['create', title]); return { number: next, html_url: `u/${next++}`, title, state: 'open' } },
    reopen: number => calls.push(['reopen', number]),
    comment: (number, body) => calls.push(['comment', number, body.split('\n')[0]]),
  }
}

test('issues are deduplicated by title: filed once, then commented on, and reopened when closed', () => {
  const flaky = [{ suite: 'a', name: 'new', attempts: 1, output: 'x' }, { suite: 'a', name: 'open', attempts: 1, output: 'y' }, { suite: 'a', name: 'closed', attempts: 1, output: 'z' }, { suite: 'a', name: 'new', attempts: 1, output: 'x' }]
  const client = fakeClient([
    { number: 1, title: 'Flaky test: a open', state: 'open', html_url: 'u/1' },
    { number: 2, title: 'Flaky test: a closed', state: 'closed', html_url: 'u/2' },
    { number: 3, title: 'Flaky test: a open (other)', state: 'open', html_url: 'u/3' },
  ])
  const results = fileIssues(flaky, client, 'run')
  assert.deepEqual(results.map(r => [r.action, r.number]), [['filed', 100], ['updated', 1], ['reopened', 2], ['updated', 100]])
  assert.deepEqual(client.calls, [['label'], ['create', 'Flaky test: a new'], ['comment', 1, 'Flaked again: run'], ['reopen', 2], ['comment', 2, 'Flaked again: run'], ['comment', 100, 'Flaked again: run']])
  assert.deepEqual(fileIssues([], client, 'run'), [])
})

test('CI files no issues: only FLAKY_FILE_ISSUES=1 on a push to main does', () => {
  assert.equal(filesIssues({ FLAKY_FILE_ISSUES: '1', GITHUB_EVENT_NAME: 'push', GITHUB_REF: 'refs/heads/main' }), true)
  for (const env of [
    { GITHUB_EVENT_NAME: 'push', GITHUB_REF: 'refs/heads/main' },
    { FLAKY_FILE_ISSUES: '1', GITHUB_EVENT_NAME: 'pull_request', GITHUB_REF: 'refs/pull/189/merge' },
    { GITHUB_EVENT_NAME: 'pull_request', GITHUB_REF: 'refs/pull/189/merge' },
    { GITHUB_EVENT_NAME: 'pull_request', GITHUB_REF: 'refs/heads/main' },
    { GITHUB_EVENT_NAME: 'push', GITHUB_REF: 'refs/heads/ci/nextest-quarantine' },
    { GITHUB_EVENT_NAME: 'push', GITHUB_REF: 'refs/tags/v1.0.0' },
    { GITHUB_EVENT_NAME: 'workflow_dispatch', GITHUB_REF: 'refs/heads/main' },
    {},
  ]) assert.equal(filesIssues(env), false, JSON.stringify(env))
  const text = summary(parseJUnit(junit), [], false)
  assert.match(text, /no issues are filed/)
  assert.match(text, /\| `kybern-daemon` `network::ports::tests::flaky_<port>` \| 1 \| not filed \|/)
})

test('a run files at most MAX_ISSUES issues; the rest are listed as over the limit', () => {
  assert.equal(MAX_ISSUES, 5)
  const flaky = Array.from({ length: 8 }, (_, i) => ({ suite: 'a', name: `t${i}`, attempts: 1, output: '' }))
  // One already has an issue: commenting on it counts toward the limit too.
  const client = fakeClient([{ number: 1, title: 'Flaky test: a t0', state: 'open', html_url: 'u/1' }])
  // The same test reported twice (two binaries) is still one issue.
  const results = fileIssues([...flaky.slice(0, 5), flaky[4], ...flaky.slice(5)], client, 'run')
  assert.deepEqual(results.map(r => r.skipped ? 'skipped' : r.action), ['updated', 'filed', 'filed', 'filed', 'filed', 'updated', 'skipped', 'skipped', 'skipped'])
  assert.equal(client.calls.filter(([call]) => call === 'create').length, 4)
  const report = { tests: 9, failures: 0, flaky: [...flaky.slice(0, 5), flaky[4], ...flaky.slice(5)] }
  assert.match(summary(report, results), /3 more flaky tests were not filed: a run files at most 5 issues/)
  assert.match(summary(report, results), /\| `a` `t7` \| 1 \| not filed \(over the limit\) \|/)
})

test('a test that failed every attempt is a failure, never filed as flaky', () => {
  const report = parseJUnit(junit)
  assert.equal(report.flaky.some(t => t.name === 'fails_twice'), false)
  const client = fakeClient([])
  fileIssues(report.flaky, client, 'run')
  assert.deepEqual(client.calls.filter(([call]) => call === 'create').map(([, title]) => title), [
    'Flaky test: kybern-daemon network::ports::tests::flaky_<port>', 'Flaky test: kybern-drivers::live_drivers error_then_pass',
  ])
})

test('an issue filed by hand that names the test is updated instead of duplicated', () => {
  const client = fakeClient([
    { number: 188, title: 'Flaky: orchestrator::tests::a_turn_ends', state: 'open', html_url: 'u/188' },
    { number: 9, title: 'Flaky: orchestrator::tests::a_turn_ends_later', state: 'open', html_url: 'u/9' },
  ])
  const results = fileIssues([
    { suite: 'kybern-daemon', name: 'orchestrator::tests::a_turn_ends', output: '' },
    { suite: 'kybern-daemon', name: 'orchestrator::tests::a_turn', output: '' },
  ], client, 'run')
  assert.deepEqual(results.map(r => [r.action, r.number]), [['updated', 188], ['filed', 100]])
})

test('a read-only token or API failure is reported per test, never thrown', () => {
  const denied = { ...fakeClient([]), ensureLabel: () => { throw Object.assign(new Error('x'), { stderr: 'HTTP 403: Resource not accessible by integration' }) } }
  assert.deepEqual(fileIssues([{ suite: 'a', name: 'b', output: '' }], denied, 'run'), [{ error: 'could not read the flaky issues (HTTP 403: Resource not accessible by integration)' }])
  const failing = { ...fakeClient([]), create: () => { throw new Error('HTTP 500\nmore') } }
  assert.deepEqual(fileIssues([{ suite: 'a', name: 'b', output: '' }], failing, 'run'), [{ error: 'HTTP 500' }])
})

test('the job summary lists each flaky test with its issue, and says when there were none', () => {
  const report = parseJUnit(junit)
  const text = summary(report, [{ action: 'filed', number: 5, url: 'https://x/5' }, { error: 'HTTP 403' }])
  assert.match(text, /2 tests failed and then passed on retry/)
  assert.match(text, /\| `kybern-daemon` `network::ports::tests::flaky_<port>` \| 1 \| \[#5\]\(https:\/\/x\/5\) filed \|/)
  assert.match(text, /\| `kybern-drivers::live_drivers` `error_then_pass` \| 2 \| not filed: HTTP 403 \|/)
  assert.match(summary({ tests: 7, failures: 0, flaky: [] }, []), /None: 7 tests ran without needing a retry/)
  assert.match(summary(null, []), /No nextest report was written/)
})

test('gh calls: paginated label listing without pull requests, JSON bodies on stdin, existing label tolerated', () => {
  const calls = []
  const client = ghClient('o/r', (args, input) => {
    calls.push([args, input])
    if (args.includes('--paginate')) return JSON.stringify([[{ number: 1, title: 't' }, { number: 2, title: 'p', pull_request: {} }]])
    if (args[3] === 'repos/o/r/labels') throw Object.assign(new Error('failed'), { stderr: 'HTTP 422: Validation Failed (already_exists)' })
    return JSON.stringify({ number: 7, html_url: 'u' })
  })
  assert.deepEqual(client.labelled().map(i => i.number), [1])
  assert.equal(calls[0][0].join(' '), 'api --paginate --slurp repos/o/r/issues?labels=flaky&state=all&per_page=100')
  client.ensureLabel()
  assert.equal(client.create('T', 'B').number, 7)
  assert.deepEqual(calls.at(-1), [['api', '--method', 'POST', 'repos/o/r/issues', '--input', '-'], JSON.stringify({ title: 'T', body: 'B', labels: ['flaky'] })])
  client.reopen(3)
  assert.deepEqual(calls.at(-1)[0], ['api', '--method', 'PATCH', 'repos/o/r/issues/3', '--input', '-'])
  const forbidden = ghClient('o/r', () => { throw Object.assign(new Error('failed'), { stderr: 'HTTP 403' }) })
  assert.throws(() => forbidden.ensureLabel())
  assert.throws(() => ghClient('not a repo'))
})
