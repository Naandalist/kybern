// When the phone's own connection should try the computer's other addresses. The phone opens the computer it is at
// on the one address it saved, but that address can stop reaching the daemon while the computer itself is fine: the
// daemon came back on another port after an update, or an old daemon that never finished shutting down still holds
// the address, answers the identity check and refuses every session. The network knows where the computer is now
// (`network.routes`, learned from any member), so once the saved address has failed for a while the phone tries those.
import type { ConnectionStatus } from "../../../../packages/kybern-client/src/client";

/** How long the saved address may keep failing before the others are tried. */
export const FAILOVER_AFTER_MS = 6_000;
/** The least time between two rounds of trying the other addresses. */
export const FAILOVER_EVERY_MS = 15_000;

export interface FailoverInput {
  status: ConnectionStatus;
  /** `auth` when the computer refused the credential: another address of the same computer refuses it too. */
  failure?: string | undefined;
  /** When the connection last stopped being open (or started connecting), in ms. */
  downSince?: number | undefined;
  /** When the last round started, in ms (0 for never). */
  lastTry: number;
  trying: boolean;
  now: number;
}

export function shouldTryOtherAddresses(input: FailoverInput): boolean {
  if (input.trying || input.failure === "auth") return false;
  if (input.status !== "reconnecting" && input.status !== "connecting" && input.status !== "failed") return false;
  if (input.lastTry && input.now - input.lastTry < FAILOVER_EVERY_MS) return false;
  // A failure other than the credential ("this address now belongs to a different environment") is final for this address.
  if (input.status === "failed") return true;
  return input.downSince !== undefined && input.now - input.downSince >= FAILOVER_AFTER_MS;
}
