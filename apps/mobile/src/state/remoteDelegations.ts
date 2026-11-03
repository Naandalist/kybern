// The computer's own record of the agents a thread delegated to other machines (ADE-121): what
// `delegations.remote_list` reports and `delegations.remote_changed` keeps current. Launch rows pass it to
// `launchState`, so a child on another machine says Done, Failed or Stopped even when this phone's copy of that
// machine's list is stale. Read when a thread opens, again on reconnect and when the computer says records were missed.
import { useSyncExternalStore } from "react";
import {
  DELEGATIONS_REMOTE_CHANGED_NOTIFICATION,
  DELEGATIONS_REMOTE_RESYNC_NOTIFICATION,
  type KybernClient,
  type RemoteChild,
} from "./protocol";
import { attachLate } from "./lateAttach";
import { mergeRemoteRecords } from "./remoteRecords";
import { onClientAttach, onEnvironmentReset, onThreadOpen } from "./runtime";

let records: ReadonlyMap<string, RemoteChild> = new Map();
/** Threads whose records were asked for, read again after a reconnect. */
const parents = new Set<string>();
const listeners = new Set<() => void>();
let client: KybernClient | null = null;
/** A computer older than `delegations.remote_list`. */
let unsupported = false;

function put(incoming: readonly RemoteChild[]) {
  const next = mergeRemoteRecords(records, incoming);
  if (next === records) return;
  records = next;
  listeners.forEach((fn) => fn());
}

/** Read one thread's records from the computer. A failed read keeps what was known. */
export async function loadRemoteRecords(parentId: string): Promise<void> {
  const active = client;
  if (!active || unsupported) return;
  parents.add(parentId);
  try {
    const { children } = await active.call("delegations.remote_list", { parent_thread_id: parentId });
    if (client === active) put(children ?? []);
  } catch (e) {
    if (/-32601|unknown method|method not found/i.test(e instanceof Error ? e.message : String(e))) unsupported = true;
  }
}

function attach(next: KybernClient, isCurrent: () => boolean) {
  client = next;
  unsupported = false;
  next.onNotification(DELEGATIONS_REMOTE_CHANGED_NOTIFICATION, (params) => {
    const record = params as RemoteChild | null;
    if (isCurrent() && record?.task_id) put([record]);
  });
  next.onNotification(DELEGATIONS_REMOTE_RESYNC_NOTIFICATION, () => {
    if (isCurrent()) for (const id of parents) void loadRemoteRecords(id);
  });
  for (const id of parents) void loadRemoteRecords(id);
}
onClientAttach(attach);
// Imported by a screen after the computer connected: adopt the live client now.
attachLate(attach, () => {});
onThreadOpen((threadId) => void loadRemoteRecords(threadId));
onEnvironmentReset(() => {
  client = null;
  parents.clear();
  records = new Map();
  listeners.forEach((fn) => fn());
});

const subscribe = (fn: () => void) => {
  listeners.add(fn);
  return () => {
    listeners.delete(fn);
  };
};

/** The computer's record of a delegated task on another machine, when it has one. */
export function useRemoteRecord(taskId: string | undefined): RemoteChild | undefined {
  return useSyncExternalStore(subscribe, () => (taskId ? records.get(taskId) : undefined));
}

/** Every record, for rows that count several launches. */
export function useRemoteRecords(): ReadonlyMap<string, RemoteChild> {
  return useSyncExternalStore(subscribe, () => records);
}
