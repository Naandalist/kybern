import assert from "node:assert/strict"
import test from "node:test"
import {
  ACCOUNT_COLORS,
  accountColorVar,
  accountDisplayName,
  accountFor,
  defaultAccountFor,
  initialLoginMode,
  isLoginLive,
  legacyAccounts,
  nextFreeColor,
  remoteNote,
  shouldFinishOnClose,
  stepFor,
  successDefaults,
} from "../lib/accounts.ts"

const login = (patch) => ({ id: "l1", kind: "claude-code", mode: "browser", phase: "waiting", expires_at: "2026-01-01T00:00:00Z", ...patch })

test("the next free color skips colors the agent already uses and cycles after six", () => {
  assert.equal(nextFreeColor([]), "blue")
  assert.equal(nextFreeColor(["blue", undefined, "purple"]), "green")
  assert.equal(nextFreeColor(["green", "blue"]), "purple")
  assert.equal(nextFreeColor(["red", null]), "blue")
  assert.equal(nextFreeColor([...ACCOUNT_COLORS]), "blue")
  assert.equal(nextFreeColor([...ACCOUNT_COLORS, "blue"]), "green")
})

test("only palette keys resolve to a color", () => {
  assert.equal(accountColorVar("teal"), "var(--account-teal)")
  assert.equal(accountColorVar("red"), null)
  assert.equal(accountColorVar(undefined), null)
})

test("the implicit instance is the CLI account and named accounts keep their name", () => {
  assert.equal(accountDisplayName("default", "Anything"), "CLI account")
  assert.equal(accountDisplayName(undefined), "CLI account")
  assert.equal(accountDisplayName("work", " Work "), "Work")
  assert.equal(accountDisplayName("gone", ""), "Account unavailable")
})

test("remote daemons route claude and omp to paste, codex to a device code and cursor to the browser", () => {
  assert.equal(initialLoginMode("claude-code", false), "browser")
  assert.equal(initialLoginMode("claude-code", true), "paste")
  assert.equal(initialLoginMode("omp", true), "terminal")
  assert.equal(initialLoginMode("omp", false), "terminal")
  assert.equal(initialLoginMode("codex", true), "device_code")
  assert.equal(initialLoginMode("codex", false), "browser")
  assert.equal(initialLoginMode("cursor", true), "browser")
  assert.equal(initialLoginMode("opencode", false), "terminal")
  assert.equal(initialLoginMode("pi", true), "terminal")
assert.equal(initialLoginMode("antigravity", false), "terminal")
assert.equal(initialLoginMode("antigravity", true), "terminal")
})

test("the remote note names the host and the transport", () => {
  assert.equal(remoteNote("studio-mac", "SSH"), "Kybern is connected to studio-mac over SSH, so the browser can't return to it directly.")
})

test("steps follow the login's mode and phase", () => {
  assert.equal(stepFor(null), "pick")
  assert.equal(stepFor(login({ phase: "starting" })), "waiting")
  assert.equal(stepFor(login({ mode: "paste" })), "paste")
  assert.equal(stepFor(login({ mode: "device_code", phase: "verifying" })), "device")
  assert.equal(stepFor(login({ mode: "terminal" })), "terminal")
  assert.equal(stepFor(login({ phase: "signed_in" })), "success")
  assert.equal(stepFor(login({ phase: "signed_in", duplicate_of: "work" })), "duplicate")
  assert.equal(stepFor(login({ phase: "failed", error: "No." })), "error")
  assert.equal(stepFor(login({ phase: "canceled" })), null)
})

test("closing saves a signed-in new account but not a sign-in again or a duplicate", () => {
  assert.equal(shouldFinishOnClose(login({ phase: "signed_in" })), true)
  assert.equal(shouldFinishOnClose(login({ phase: "signed_in", instance: "work" })), false)
  assert.equal(shouldFinishOnClose(login({ phase: "signed_in", duplicate_of: "work" })), false)
  assert.equal(shouldFinishOnClose(login({ phase: "waiting" })), false)
  assert.equal(isLoginLive(login({ phase: "failed" })), false)
  assert.equal(isLoginLive(login({ phase: "waiting" })), true)
})

test("the success step prefills the suggested name and a free color", () => {
  assert.deepEqual(successDefaults(login({ phase: "signed_in", suggested_name: "Arunika", suggested_color: "green" }), ["blue"]), { name: "Arunika", color: "green" })
  assert.deepEqual(successDefaults(login({ phase: "signed_in", suggested_name: "Arunika", suggested_color: "blue" }), ["blue"]), { name: "Arunika", color: "green" })
  assert.equal(successDefaults(login({ phase: "signed_in" }), []).name, "New account")
})

test("older daemons list the CLI account and the named accounts from settings", () => {
  const providers = { "claude-code": { env: {}, default_account: "work", project_accounts: { "/p": "work" }, accounts: { work: { name: "Work", directory: "/d", color: "blue", email: "w@x.co" } } } }
  const accounts = legacyAccounts(providers, ["claude-code", "codex"])
  assert.deepEqual(accounts.map((a) => [a.provider.kind, a.provider.instance, a.is_default]), [
    ["claude-code", "default", false],
    ["claude-code", "work", true],
    ["codex", "default", true],
  ])
  const work = accountFor(accounts, "claude-code", "work")
  assert.equal(work.identity.email, "w@x.co")
  assert.deepEqual(work.projects, ["/p"])
  assert.equal(accountFor(accounts, "claude-code", undefined).name, "CLI account")
  assert.equal(defaultAccountFor(accounts, "claude-code").provider.instance, "work")
  assert.equal(defaultAccountFor(accounts, "codex").provider.instance, "default")
})

test("a late poll cannot pull a signed-in login back, but a new error gets through", async () => {
  const { acceptLoginUpdate, loginHost } = await import("../lib/accounts.ts")
  const waiting = login({ phase: "waiting" })
  const done = login({ phase: "signed_in" })
  assert.equal(acceptLoginUpdate(done, waiting), done)
  assert.equal(acceptLoginUpdate(waiting, done), done)
  const wrongCode = login({ phase: "waiting", error: "That code didn't work." })
  assert.equal(acceptLoginUpdate(waiting, wrongCode), wrongCode)
  assert.equal(acceptLoginUpdate(done, login({ id: "l2", phase: "starting" })).id, "l2")
  assert.equal(loginHost("https://www.claude.com/cai/oauth/authorize?x=1"), "claude.com")
  assert.equal(loginHost("nonsense"), null)
})
