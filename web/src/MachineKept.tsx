import type { MachineHeld } from "./api";
import type { MachineView } from "./fleet";

/**
 * "Kept on this machine" on a machine's page: the GitHub logins and gateway keys it holds in its
 * own secret store. Renders nothing yet; a later change fills it.
 */
export function MachineKept(props: { machine: MachineView; held?: MachineHeld; protocols: Record<string, number>; here?: string }) {
  void props;
  return null;
}
