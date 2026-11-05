// Actionable errors: the one place that turns any failure into the words a person reads. It prefers the daemon's
// `data.problem` (kind + subject); for older daemons and client-side failures it sorts the error itself (an offline
// machine, a connection that closed, a machine error code, the text of a gh, git or permission failure). Headlines never
// contain "RpcCallError", "invalid params", "not found:", exit codes or paths; the technical text is only `detail`.
import { classifyPrError } from "./prReview.ts"
import { errorMessage, problemOf } from "./problem.ts"
import type { JsonValue, Problem, ProblemKind } from "./types.ts"

/** What the person was doing when it failed. */
export interface ProblemContext {
  /** A verb phrase for "Couldn't …": "load pull requests", "send your message", "rename the thread". */
  action: string
  /** What a setup fix unlocks, for "Install GitHub CLI to …"; defaults to `action`. */
  goal?: string
  /** The machine the call ran on, when it isn't this one: "SatyaMas". */
  machine?: string
}

export interface ProblemViewAction {
  /** What the client does: `install_tool`, `sign_in`, `retry`, `open_machines`, `copy_detail`, `update`, … */
  id: string
  label: string
  params?: { [key: string]: JsonValue }
}

export interface ProblemView {
  kind: ProblemKind
  subject?: string
  /** The headline: the outcome or the fix, sentence case, no final period. */
  title: string
  /** One sentence of why or what next. */
  body: string
  action?: ProblemViewAction
  secondary?: ProblemViewAction
  /** The technical text, verbatim, for Details and Copy. */
  detail: string
  retryable: boolean
}

const TOOL_LABELS: Record<string, string> = {
  gh: "GitHub CLI",
  git: "Git",
  tailscale: "Tailscale",
  node: "Node.js",
  npm: "Node.js",
  npx: "Node.js",
  claude: "Claude Code",
  codex: "Codex",
  opencode: "OpenCode",
  cursor: "Cursor",
  "cursor-agent": "Cursor",
  agy: "Antigravity",
  antigravity: "Antigravity",
  grok: "Grok Build",
  pi: "Pi",
  omp: "Oh My Pi",
}

/** Why Kybern needs a tool, for the tools people meet first. */
const TOOL_WHY: Record<string, string> = {
  gh: "Kybern uses your own GitHub sign-in through the GitHub CLI (gh). Install it once and every project can use it.",
  git: "Kybern uses Git for checkpoints, commits and worktrees.",
  tailscale: "Kybern links your computers through your own Tailscale network.",
  node: "Install Node.js 20 or newer, then try again.",
}

export function toolLabel(tool: string): string {
  return TOOL_LABELS[tool] ?? tool
}

const retry = (label = "Try again"): ProblemViewAction => ({ id: "retry", label })
const copyDetail: ProblemViewAction = { id: "copy_detail", label: "Copy details" }

/** The words, fix and details for `error`, whatever it is. */
export function describeProblem(error: unknown, context: ProblemContext): ProblemView {
  const message = errorMessage(error)
  const problem = problemOf(error) ?? legacyProblem(error, message)
  const detail = problem.detail ?? message
  const view = copyFor(problem, message, context)
  return { kind: problem.kind, ...(problem.subject ? { subject: problem.subject } : {}), ...view, detail, retryable: problem.retryable ?? view.retryable }
}

type Copy = Pick<ProblemView, "title" | "body" | "action" | "secondary" | "retryable">

function copyFor(problem: Problem, message: string, context: ProblemContext): Copy {
  const where = context.machine ?? "this computer"
  const goal = context.goal ?? context.action
  const plain = plainSentence(message)
  const subject = problem.subject
  switch (problem.kind) {
    case "tool_missing": {
      if (!subject) {
        const body = plainSentence(problem.hint ?? "") ?? plain ?? `A program Kybern needs isn't installed on ${where}. Install it, then try again.`
        return { title: problem.title ?? `Couldn't ${context.action}`, body, secondary: retry("Check again"), retryable: true }
      }
      const label = toolLabel(subject)
      const why = TOOL_WHY[subject] ?? `${label} isn't installed on ${where}, or Kybern can't find it.`
      return {
        title: `Install ${label} to ${goal}`,
        body: context.machine ? `${label} isn't installed on ${context.machine}. ${why}` : why,
        action: { id: "install_tool", label: `Install ${label}`, params: { tool: subject } },
        secondary: retry("Check again"),
        retryable: true,
      }
    }
    case "signed_out":
      if (subject === "gh")
        return {
          title: `Sign in to GitHub to ${goal}`,
          body: "The GitHub CLI (gh) is installed but isn’t signed in. Kybern uses your own GitHub sign-in through it.",
          action: { id: "sign_in", label: "Sign in to GitHub", params: { provider: "gh" } },
          secondary: retry("Check again"),
          retryable: true,
        }
      return {
        title: subject ? `${toolLabel(subject)} is signed out` : `Sign in to ${goal}`,
        body: plain ?? "Sign in, then try again.",
        action: { id: "sign_in", label: "Sign in", params: subject ? { provider: subject } : {} },
        secondary: retry(),
        retryable: true,
      }
    case "permission":
      if (subject === "gh")
        return { title: "GitHub refused access", body: "The signed-in GitHub account can’t see or change this repository. Check its access on GitHub, then try again.", secondary: retry(), retryable: true }
      return { title: `Kybern isn't allowed to ${context.action}`, body: plain ?? `${capitalize(where)} refused access. Check the permissions, then try again.`, action: retry(), secondary: copyDetail, retryable: true }
    case "offline": {
      const machine = subject ?? context.machine
      return {
        title: machine ? `${machine} is offline` : `Can't reach Kybern to ${context.action}`,
        body: machine ? "Kybern keeps trying. Make sure that computer is awake and online." : "Check the connection. Kybern keeps trying.",
        action: retry(),
        secondary: { id: "open_machines", label: "Machine settings" },
        retryable: true,
      }
    }
    case "conflict":
      return { title: `Couldn't ${context.action}`, body: plain ?? "Something changed at the same time. Refresh, then try again.", action: retry(), retryable: true }
    case "invalid_input":
      return { title: `Couldn't ${context.action}`, body: plain ?? "Kybern didn't accept that. Check what you entered, then try again.", secondary: copyDetail, retryable: false }
    case "not_found":
      if (subject === "github_remote") return { title: "This project isn’t on GitHub", body: "Add a GitHub remote to this repository, then try again.", secondary: retry(), retryable: true }
      if (subject === "gh") return { title: "GitHub couldn’t find this", body: "Check that origin points at GitHub and that the signed-in account can see the repository, then try again.", secondary: retry(), retryable: true }
      return { title: `Couldn't ${context.action}`, body: notFoundBody(subject, plain), action: retry("Refresh"), retryable: false }
    case "update_needed":
      return { title: `Update Kybern to ${goal}`, body: plain ?? `This needs a newer Kybern on ${where}.`, action: { id: "update", label: "Update" }, retryable: false }
    case "internal":
    default:
      return {
        title: `Couldn't ${context.action}`,
        body: plain ?? "Try again. If it keeps happening, copy the details and report them.",
        action: retry(),
        secondary: copyDetail,
        retryable: true,
      }
  }
}

function notFoundBody(subject: string | undefined, plain: string | null): string {
  if (subject === "terminal") return "This terminal has closed. Open a new one."
  if (subject === "thread") return "This thread was deleted or moved. Refresh, then try again."
  if (subject === "project") return "This project was removed or moved. Refresh, then try again."
  return plain ?? "It may have been deleted or moved. Refresh, then try again."
}

const capitalize = (text: string) => text.charAt(0).toUpperCase() + text.slice(1)

/**
 * `text` when it already reads as one or two plain sentences a person can act on, else `null`. Wire jargon, class
 * names, exit codes, OS error numbers, stack frames and file paths are technical: they belong in Details.
 */
export function plainSentence(text: string): string | null {
  const t = text.trim()
  if (!t || t.length > 240 || t.includes("\n")) return null
  if (/RpcCallError|invalid params|^not found:|method not found|missing scope|os error \d|exit (code|status)|errno|EPERM|EACCES|ENOENT|panicked|\bat .+:\d+:\d+|^\w*Error:|^run \S+:|^\S+ \S+.*failed:|program not found|\(code -?\d+\)/i.test(t)) return null
  if (/[A-Za-z]:\\|\\\\\?\\|(^|[\s"'(])\/(Users|home|usr|tmp|var|opt|private|etc)\//.test(t)) return null
  return /[.!?]$/.test(t) ? t : `${t}.`
}

/** The problem an error from an older daemon, or from the client itself, most likely is. */
function legacyProblem(error: unknown, message: string): Problem {
  const name = error && typeof error === "object" ? (error as { name?: unknown }).name : undefined
  if (name === "OfflineError" || name === "ConnectionClosedError") return { kind: "offline", detail: message, retryable: true }
  const code = error && typeof error === "object" ? (error as { code?: unknown }).code : undefined
  if (typeof code === "string") {
    const machine = machineProblem(code, message)
    if (machine) return machine
  }
  const text = textProblem(message)
  if (text) return text
  if (typeof code === "number") return { kind: kindForCode(code), detail: message, retryable: true }
  return { kind: "internal", detail: message, retryable: true }
}

/** A `MachineError` code from the machine set (`machines.ts`). */
function machineProblem(code: string, detail: string): Problem | null {
  switch (code) {
    case "unreachable":
    case "no_route":
    case "refresh_failed":
      return { kind: "offline", detail, retryable: true }
    case "identity_mismatch":
    case "unpinned":
    case "insecure_route":
      return { kind: "permission", subject: "machine", detail, retryable: false }
    case "unpaired":
      return { kind: "signed_out", subject: "machine", detail, retryable: true }
    case "clock":
      return { kind: "internal", subject: "clock", detail, retryable: true }
    default:
      return null
  }
}

/** Sorts the text older daemons sent, most specific first. */
function textProblem(text: string): Problem | null {
  const detail = text
  const prClass = classifyPrError(text)
  if (/(^|\b)gh\b|GitHub CLI/.test(text) && prClass === "gh-missing") return { kind: "tool_missing", subject: "gh", detail, retryable: true }
  if (prClass === "gh-auth") return { kind: "signed_out", subject: "gh", detail, retryable: true }
  const spawn = /(?:^|: )run ([\w.-]+)(?:: (?:program not found|No such file or directory)|$)/im.exec(text) ?? /([\w.-]+): (?:program not found|command not found)/i.exec(text)
  if (spawn) return { kind: "tool_missing", subject: shortTool(spawn[1]), detail, retryable: true }
  if (/isn['’]t installed or isn['’]t on PATH/i.test(text)) {
    const tool = /^(\S+) isn['’]t installed/i.exec(text)?.[1]
    return { kind: "tool_missing", ...(tool && !tool.includes(" ") ? { subject: shortTool(tool) } : {}), detail, retryable: true }
  }
  const named = /\b(Tailscale|Git|Node\.js|GitHub CLI) (?:is not|isn['’]t) installed/i.exec(text)
  if (named) {
    const tool = { tailscale: "tailscale", git: "git", "node.js": "node", "github cli": "gh" }[named[1].toLowerCase()]
    return { kind: "tool_missing", ...(tool ? { subject: tool } : {}), detail, retryable: true }
  }
  if (/comes with Node\.js|Install Node\.js/i.test(text)) return { kind: "tool_missing", subject: "node", detail, retryable: true }
  if (/EPERM|EACCES|operation not permitted|permission denied|Access is denied|os error 5\b/i.test(text)) return { kind: "permission", detail, retryable: true }
  if (/\bis offline\b|\bis paused\b|Network request failed|Connection (timed out|closed|refused)/i.test(text)) {
    const machine = /^(.+?) is (?:offline|paused)\b/i.exec(text)?.[1]
    return { kind: "offline", ...(machine && machine.length <= 64 ? { subject: machine } : {}), detail, retryable: true }
  }
  const notFound = /^not found: (\w+)/i.exec(text)
  if (notFound) return { kind: "not_found", subject: notFound[1], detail, retryable: false }
  if (/^invalid params:/i.test(text)) return { kind: "invalid_input", detail, retryable: false }
  if (/^method not found:/i.test(text)) return { kind: "update_needed", detail, retryable: false }
  return null
}

function shortTool(program: string): string {
  return program.split(/[\\/]/).pop()!.toLowerCase().replace(/\.(exe|cmd|bat|ps1)$/, "")
}

/** The kind an error with only a JSON-RPC code most likely is (the daemon's `ProblemKind::for_code`). */
function kindForCode(code: number): ProblemKind {
  switch (code) {
    case -32700:
    case -32600:
    case -32602:
      return "invalid_input"
    case -32601:
      return "update_needed"
    case -32001:
    case -32002:
      return "permission"
    case -32003:
      return "not_found"
    case -32004:
    case -32012:
      return "conflict"
    default:
      return "internal"
  }
}
