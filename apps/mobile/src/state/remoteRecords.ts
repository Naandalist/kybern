// Pure helpers for the computer's records of children on other machines (`remoteDelegations.ts`).
import type { RemoteChild } from "../../../../packages/kybern-client/src/types";

const time = (value: string | undefined) => {
  const at = value ? Date.parse(value) : NaN;
  return Number.isFinite(at) ? at : 0;
};

/**
 * `records` with `incoming` laid over it, by task. A record older than the one held is skipped, so a read answered after
 * a notification cannot put an older state back. Returns `records` itself when nothing changed.
 */
export function mergeRemoteRecords(records: ReadonlyMap<string, RemoteChild>, incoming: readonly RemoteChild[]): ReadonlyMap<string, RemoteChild> {
  let next: Map<string, RemoteChild> | null = null;
  for (const record of incoming) {
    if (!record?.task_id) continue;
    const held = (next ?? records).get(record.task_id);
    if (held && (held === record || time(held.updated_at) > time(record.updated_at))) continue;
    next ??= new Map(records);
    next.set(record.task_id, record);
  }
  return next ?? records;
}
