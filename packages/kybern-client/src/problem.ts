// Actionable errors: the `Problem` a daemon attaches to an RPC error as `data.problem`, read the same way by every client.
import type { Problem, ProblemKind } from "./types.ts"

const KINDS: ReadonlySet<string> = new Set<ProblemKind>([
  "tool_missing", "signed_out", "permission", "offline", "conflict", "invalid_input", "not_found", "update_needed", "internal",
])

/** The `data.problem` of a daemon error (an `RpcCallError` or anything with `data`), or `null` when it has none Kybern knows. */
export function problemOf(error: unknown): Problem | null {
  const data = error && typeof error === "object" ? (error as { data?: unknown }).data : undefined
  const problem = data && typeof data === "object" ? (data as { problem?: unknown }).problem : undefined
  if (!problem || typeof problem !== "object") return null
  const kind = (problem as { kind?: unknown }).kind
  return typeof kind === "string" && KINDS.has(kind) ? (problem as Problem) : null
}

/** An error's own message, without the `RpcCallError: ` that `String(error)` puts in front of it. */
export function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message
  if (error && typeof error === "object" && typeof (error as { message?: unknown }).message === "string") return (error as { message: string }).message
  return String(error).replace(/^RpcCallError: /, "")
}

/** What a call rejected with, as an `Error` that keeps its `data.problem` (anything else becomes an `Error` of its text). */
export function asError(error: unknown): Error {
  return error instanceof Error ? error : new Error(errorMessage(error))
}
