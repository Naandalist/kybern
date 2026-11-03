// Whether Home can work. The computer the phone is at (its own connection) answering is the usual case. With several
// machines the phone also reaches the others itself, so one computer that stopped answering does not turn Home into
// the "Open connections" screen while the rest of the network is up: threads, spaces and new chats go through them,
// and a new chat for the computer that is away waits in a member's outbox.
import type { ConnectionStatus } from "../../../../packages/kybern-client/src/client";
import { machineReachable } from "../../../../packages/kybern-client/src/machineTarget";
import type { MachineSummary } from "../../../../packages/kybern-client/src/machines";

export interface HomeReach {
  /** The computer the phone is at answers. */
  connected: boolean;
  /** Home works: that computer answers, or another machine of the network does. */
  reachable: boolean;
}

export function homeReach(status: ConnectionStatus, machines: readonly Pick<MachineSummary, "status" | "isLocal">[]): HomeReach {
  const connected = status === "open";
  const network = machines.length > 1 && machines.some((machine) => !machine.isLocal && machineReachable(machine.status));
  return { connected, reachable: connected || network };
}
