// Pull request review drafts: durable text, inline anchors and selected findings bound to the
// commit they were written against. Pure; shared by desktop, GPUIX and mobile so a draft and
// its repair prompt mean the same thing in every client. (Moved from desktop state/prReviewModel.ts.)
import type {
  PrPageKind,
  PrPageResult,
  PrInlineComment,
  PrReviewEntry,
  Problem,
} from "./types.ts"
import { errorMessage, problemOf } from "./problem.ts"

export type ReviewWorkspace = "overview" | "changes" | "conversation"
export function reviewWorkspaceKind(
  workspace: ReviewWorkspace,
  current: PrPageKind
): PrPageKind {
  if (workspace === "changes") return "files"
  if (workspace === "conversation")
    return ["comments", "reviews", "review_comments"].includes(current)
      ? current
      : "comments"
  return current
}
export function reviewOverviewCache(
  previousHead: string | null,
  currentHead: string,
  page: PrPageResult | null,
  file: string | null
) {
  return previousHead === currentHead
    ? { page, file }
    : { page: null, file: null }
}

export interface ReviewDraft {
  /** Commit whose visible diff supplied this review text and inline anchors. */
  sourceHead: string | null
  body: string
  inline: PrInlineComment[]
  pendingInline: PrInlineComment | null
  selected: string[]
  findings: Record<string, PrReviewEntry>
  threadId: string | null
}
export const emptyReviewDraft = (): ReviewDraft => ({
  sourceHead: null,
  body: "",
  inline: [],
  pendingInline: null,
  selected: [],
  findings: {},
  threadId: null,
})

export function hasReviewDraftContent(draft: ReviewDraft): boolean {
  return !!draft.body || draft.inline.length > 0 || draft.pendingInline !== null
}

export function reviewDraftIsStale(
  draft: ReviewDraft,
  currentHead: string
): boolean {
  return hasReviewDraftContent(draft) && draft.sourceHead !== currentHead
}

/** Editing an existing draft never silently moves its anchors to a newer head. */
export function bindReviewDraft(
  current: ReviewDraft,
  patch: Partial<ReviewDraft>,
  visibleHead: string | null
): ReviewDraft {
  const next = { ...current, ...patch }
  if (!hasReviewDraftContent(next)) next.sourceHead = null
  else if (!("sourceHead" in patch) && !hasReviewDraftContent(current))
    next.sourceHead = visibleHead
  return next
}

export function reuseReviewDraftText(
  draft: ReviewDraft,
  currentHead: string
): ReviewDraft {
  if (draft.inline.length || draft.pendingInline)
    throw new Error(
      "Remove old inline drafts and their unfinished editor before using text for the current commit. Choose new lines to re-anchor those comments."
    )
  return bindReviewDraft(draft, { sourceHead: currentHead }, currentHead)
}

export function clearReviewDraft(draft: ReviewDraft): ReviewDraft {
  return {
    ...draft,
    sourceHead: null,
    body: "",
    inline: [],
    pendingInline: null,
  }
}

export function assertReviewDraftHead(draft: ReviewDraft, currentHead: string) {
  if (reviewDraftIsStale(draft, currentHead))
    throw new Error(
      "This draft belongs to a different commit. Review the current changes, then reuse its text or clear it and choose new inline anchors before submitting."
    )
}

/** A successful submission removes only the exact content that was sent.
 * Typing during a slow request must remain a new draft; failure changes nothing. */
export function afterReviewSubmission(
  current: ReviewDraft,
  submitted: ReviewDraft
): ReviewDraft {
  const sent = new Set(
    submitted.inline.map((comment) => JSON.stringify(comment))
  )
  return {
    ...current,
    body: current.body === submitted.body ? "" : current.body,
    inline: current.inline.filter(
      (comment) => !sent.has(JSON.stringify(comment))
    ),
  }
}
export async function submitReviewDraft(
  submitted: ReviewDraft,
  publish: () => Promise<unknown>,
  read: () => ReviewDraft,
  save: (draft: ReviewDraft) => void,
  currentHead?: string
) {
  if (currentHead !== undefined) assertReviewDraftHead(submitted, currentHead)
  await publish()
  save(afterReviewSubmission(read(), submitted))
}

export function selectedReviewFindings(draft: ReviewDraft): PrReviewEntry[] {
  return draft.selected
    .map((id) => draft.findings[id])
    .filter((entry): entry is PrReviewEntry => !!entry)
}
export function toggleReviewFinding(
  draft: ReviewDraft,
  id: string,
  entry: PrReviewEntry
): ReviewDraft {
  if (!draft.selected.includes(id) && draft.selected.length >= 100) return draft
  const selected = draft.selected.includes(id)
    ? draft.selected.filter((value) => value !== id)
    : [...draft.selected, id]
  const findings = { ...draft.findings }
  if (selected.includes(id)) findings[id] = entry
  else delete findings[id]
  return { ...draft, selected, findings }
}

/** Loading a local draft treats malformed data as absent, never as executable input. */
export function readReviewDraft(value: unknown): ReviewDraft {
  if (!value || typeof value !== "object") return emptyReviewDraft()
  const draft = value as Partial<ReviewDraft>
  const validComment = (comment: unknown): comment is PrInlineComment => {
    if (!comment || typeof comment !== "object") return false
    const c = comment as PrInlineComment
    return (
      typeof c.path === "string" &&
      Number.isInteger(c.line) &&
      c.line > 0 &&
      (c.side === "LEFT" || c.side === "RIGHT") &&
      typeof c.body === "string" &&
      c.body.length <= 65536
    )
  }
  const findings: Record<string, PrReviewEntry> = {}
  const selected: string[] = []
  for (const id of Array.isArray(draft.selected)
    ? draft.selected.slice(0, 100)
    : []) {
    const entry = draft.findings?.[id]
    if (
      typeof id === "string" &&
      entry &&
      typeof entry.body === "string" &&
      typeof entry.author === "string" &&
      entry.body.length <= 65536
    ) {
      findings[id] = entry
      selected.push(id)
    }
  }
  return {
    sourceHead:
      typeof draft.sourceHead === "string" && draft.sourceHead.length <= 128
        ? draft.sourceHead
        : null,
    body: typeof draft.body === "string" ? draft.body.slice(0, 65536) : "",
    inline: Array.isArray(draft.inline)
      ? draft.inline.filter(validComment).slice(0, 100)
      : [],
    pendingInline: validComment(draft.pendingInline)
      ? draft.pendingInline
      : null,
    selected,
    findings,
    threadId: typeof draft.threadId === "string" ? draft.threadId : null,
  }
}

export function repairReviewPrompt(
  number: number,
  title: string,
  url: string,
  head: string,
  draft: ReviewDraft
): string {
  const findings = [
    ...selectedReviewFindings(draft).map(
      (entry) =>
        `${entry.path ? `${entry.path}:${entry.line ?? "?"} (${entry.side ?? "RIGHT"})` : `Review by ${entry.author}`}\n${entry.body}`
    ),
    ...draft.inline.map(
      (entry) => `${entry.path}:${entry.line} (${entry.side})\n${entry.body}`
    ),
  ]
  const body = `Repair pull request #${number}: ${title}\n${url}\nReviewed head: ${head}\n\nAddress these selected review findings and verify the changes:\n\n${findings.join("\n\n")}\n\nKeep the repair on this pull request’s branch. Report the changes and validation. Leave publishing reviews, closing and merging to the human.`
  if (new TextEncoder().encode(body).byteLength > 64 * 1024)
    throw new Error(
      "These findings exceed the 64 KiB repair limit. Select fewer findings or shorten your inline drafts, then send again."
    )
  return body
}

export type PrErrorClass = "gh-missing" | "gh-auth" | "no-remote" | "other"
/** Sorts a daemon error into the recovery the user needs. */
export function classifyPrError(text: string): PrErrorClass {
  // `run gh` alone is what daemons before 0.21.2 said when gh wasn't on their PATH.
  if (/command not found|No such file|program not found|gh: not found|GitHub CLI \(gh\) isn['’]t installed|(^|: )run gh$/im.test(text))
    return "gh-missing"
  if (/gh auth login|not logged in|authentication required|HTTP 401|gh isn['’]t signed in/i.test(text))
    return "gh-auth"
  if (/no git remotes|not a git repository|could not find|no such remote|no GitHub remote/i.test(text))
    return "no-remote"
  return "other"
}

/**
 * The `Problem` of a pull request or Actions call that failed: the daemon's `data.problem`, else one sorted from the
 * message for daemons that send none. Always has a `detail` with the technical text.
 */
export function ghProblem(error: unknown): Problem {
  const text = errorMessage(error)
  const sent = problemOf(error)
  if (sent) return { ...sent, detail: sent.detail ?? text }
  const retry = { id: "retry", label: "Check again" }
  switch (classifyPrError(text)) {
    case "gh-missing":
      return { kind: "tool_missing", subject: "gh", action: { id: "install_tool", label: "Install GitHub CLI", params: { tool: "gh" } }, secondary: retry, detail: text, retryable: true }
    case "gh-auth":
      return { kind: "signed_out", subject: "gh", action: { id: "sign_in", label: "Sign in to GitHub", params: { provider: "gh" } }, secondary: retry, detail: text, retryable: true }
    case "no-remote":
      return { kind: "not_found", subject: "github_remote", detail: text, retryable: true }
    default:
      if (/HTTP 403/i.test(text)) return { kind: "permission", subject: "gh", detail: text, retryable: true }
      if (/HTTP 404/i.test(text)) return { kind: "not_found", subject: "gh", detail: text, retryable: true }
      return { kind: "internal", detail: text, retryable: true }
  }
}

/** Whether `problem` is a GitHub CLI that has to be installed or signed in before anything loads. */
export const isGhSetup = (problem: Problem): boolean => problem.subject === "gh" && (problem.kind === "tool_missing" || problem.kind === "signed_out")
