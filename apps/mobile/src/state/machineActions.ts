// What the Machines screen does through a member of the network (the phone is a
// client with `network:admin`, so it asks the computer it is at, or the nearest
// live member, to change the network). The machine set follows through the
// network's own notifications; each action also asks it to read the network again.
import type {
  FoundMachine,
  MachineCard,
  DoctorReport,
  MachineId,
  MachineInfo,
  NetworkAddResult,
  NetworkUpdateAllResult,
  NetworkStatus,
  TokenInfo,
} from "../../../../packages/kybern-client/src/types";
import { parityOn } from "../machines/gate";
import { machineHub } from "./runtime";

export class NoMemberError extends Error {
  constructor() {
    super("Connect to a computer on your network first. Machines are managed through it.");
    this.name = "NoMemberError";
  }
}

/** True when a live machine can manage the network for this phone. */
export const canManageNetwork = () => !!machineHub().memberClient();

function member(except?: MachineId) {
  const client = machineHub().memberClient(except);
  if (!client) throw new NoMemberError();
  return client;
}

const follow = async <T>(result: T): Promise<T> => {
  void machineHub().set.syncMembers().catch(() => {});
  return result;
};

export async function networkStatus(): Promise<NetworkStatus> {
  return member().call("network.status", {});
}
export async function addFoundMachine(found: FoundMachine): Promise<NetworkAddResult> {
  return follow(await member().call(found.action === "merge" ? "network.merge" : "network.add", { found_id: found.found_id }) as NetworkAddResult);
}
/** The machine at `address` shows a six-digit code; a member joins it with that code. */
export async function joinWithCode(code: string, address: string): Promise<MachineCard> {
  return follow(await member().call("network.join", { code: code.trim(), address: address.trim() }));
}
/** Remove a machine from the network everywhere. `stolen` also marks it lost. */
export async function removeFromNetwork(machineId: MachineId, stolen: boolean): Promise<void> {
  // Never ask the machine being removed to remove itself.
  await member(machineId).call("network.remove", { machine_id: machineId, ...(stolen ? { stolen: true } : {}) });
  void machineHub().set.syncMembers().catch(() => {});
}

// ---- ADE-99: what a member of the network can be asked beyond adding and removing machines ----
// Every call below goes through `member()`; the screens show it only when `memberParity()` says that member is new enough.

/** The member that answers network calls for this phone, by id and name. */
export function memberMachine(): { machineId: MachineId; name: string; version?: string } | undefined {
  // Screens call this while they render. It must not throw for an offline machine (`clientFor` does), or the
  // Machines page goes blank and the app closes as soon as one machine is away.
  const connection = machineHub().memberConnection();
  return connection ? { machineId: connection.record.machineId, name: connection.record.name, ...(connection.info?.version ? { version: connection.info.version } : {}) } : undefined;
}

/** The version gate for member-backed rows: the member is the machine that will do the work. */
export const memberParity = (): boolean => {
  const found = memberMachine();
  return !!found && parityOn(found.version);
};

/** Whether the machine named runs a version the network controls work on. */
export const machineParity = (machineId: MachineId | undefined): boolean =>
  !!machineId && parityOn(machineHub().set.connection(machineId)?.info?.version);

/** Members older than the gate, by name, for the one footer line on Machines. */
export function olderMembers(): string[] {
  return machineHub()
    .set.connections()
    .filter((c) => c.record.member && c.status !== "offline" && c.status !== "unpaired" && c.status !== "paused" && !parityOn(c.info?.version))
    .map((c) => c.record.name);
}

export async function renameMachine(machineId: MachineId, name: string): Promise<void> {
  await follow(await member().call("network.rename", { machine_id: machineId, name: name.trim() }));
}
/** Pause or resume a machine for the whole network (the phone's own Disconnect is `hub.setPaused`). */
export async function pauseOnNetwork(machineId: MachineId, paused: boolean): Promise<void> {
  await follow(await member().call("network.pause", { machine_id: machineId, paused }));
}
export async function setNetworkEnabled(enabled: boolean): Promise<void> {
  await follow(await member().call("network.set_enabled", { enabled }));
}
export async function setFollowUpdates(enabled: boolean): Promise<void> {
  await follow(await member().call("network.follow_updates.set", { enabled }));
}
export async function updateAllMachines(): Promise<NetworkUpdateAllResult> {
  return follow(await member().call("network.update_all", {}));
}
export async function checkNetwork(): Promise<DoctorReport> {
  return member().call("network.doctor", { machine_id: null });
}

/** One machine's `machines.info`, asked of that machine (an offline one rejects, never throws: screens call it from effects). */
export async function readMachineInfo(machineId: MachineId): Promise<MachineInfo> {
  return machineHub().clientFor(machineId).call("machines.info", {});
}

const LIVE_STATES = new Set(["online", "relaying", "needs-update"]);
/** Machines the phone can ask right now. */
export const liveMachines = () => machineHub().set.connections().filter((c) => LIVE_STATES.has(c.status));

/**
 * Tokens (paired devices) of every live machine that lets this phone read them. A phone pairing carries no `access:*`
 * scope on purpose (paired devices are managed on the computer, `Scope::CLIENT` plus `network:admin`), so for a normal
 * phone this asks nobody and the section stays hidden; a full-access token sees them.
 */
export async function listPairedDevices(): Promise<{ machineId: MachineId; machineName: string; token: TokenInfo }[]> {
  const hub = machineHub();
  const live = liveMachines().filter((c) => parityOn(c.info?.version) && (c.info?.scopes ?? []).includes("access_read"));
  const results = await Promise.allSettled(live.map((c) => hub.clientFor(c.record.machineId).call("access.tokens.list", {})));
  return results.flatMap((result, index) =>
    result.status === "fulfilled"
      ? result.value.tokens.map((token) => ({ machineId: live[index]!.record.machineId, machineName: live[index]!.record.name, token }))
      : [],
  );
}
export async function revokeDeviceTokens(tokens: readonly { machineId: MachineId; token: TokenInfo }[]): Promise<void> {
  const results = await Promise.allSettled(tokens.map((t) => machineHub().clientFor(t.machineId).call("access.tokens.revoke", { token_id: t.token.id })));
  const failed = results.find((r) => r.status === "rejected");
  if (failed && failed.status === "rejected") throw failed.reason;
}
