import { post, refused, type Limits } from "./api";
import { plural } from "./labels";

/** Renames machine `id` through the daemon, which sends the name to the acting coordinator. */
export async function renameMachine({ id, name }: { id: string; name: string }): Promise<void> {
  const { status, json } = await post(`/api/machines/${id}/rename`, { name });
  if (status !== 204) throw refused(status, json);
}

/** Bytes as gigabytes for the card's line: at most three decimals. */
const gbShown = (bytes: number) => Number((bytes / 1e9).toFixed(3));

/** A machine's limits in a line. */
export const limitsText = (l: Limits) => `At most ${plural(l.max_sessions, "session")} · load ${l.max_load_per_core} per core · ${gbShown(l.min_free_disk)} GB disk free`;

/** What the limits dialog sends: the fields as numbers (a blank one is null, which the daemon refuses), or a clear. */
export type LimitsAction = { fields: { max_sessions: number | null; max_load_per_core: number | null; min_free_disk_gb: number | null } } | "clear";

export async function sendLimits({ id, action }: { id: string; action: LimitsAction }): Promise<void> {
  const { status, json } = action === "clear" ? await post(`/api/machines/${id}/limits/clear`) : await post(`/api/machines/${id}/limits`, action.fields);
  if (status !== 204) throw refused(status, json);
}

/** Removes machine `id` from the fleet: every machine then revokes its key and closes its links. */
export async function removeMachine(id: string): Promise<void> {
  const { status, json } = await post(`/api/machines/${encodeURIComponent(id)}/remove`);
  if (status !== 204) throw refused(status, json);
}

/**
 * What an add answered: the command to run on the new machine, how long its code works, and the
 * address the name resolved to.
 */
export interface Added {
  name: string;
  command: string;
  expiresInSecs: number;
  address: string;
}

/** Asks the daemon for a join code for the machine Tailscale calls `name`. */
export async function addMachine(name: string): Promise<Added> {
  const { status, json } = await post("/api/machines/add", { name });
  if (status !== 201) throw refused(status, json);
  return { name, command: String(json.command), expiresInSecs: Number(json.expires_in_secs), address: String(json.address) };
}

/** Sets the order the listed machines take the coordinator role in: the whole order, first first. */
export async function movePriority(order: string[]): Promise<void> {
  const { status, json } = await post("/api/machines/priority", { order });
  if (status !== 204) throw refused(status, json);
}
