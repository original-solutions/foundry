import { closeTerminal, get, post, refused } from "../api";

/** What an item is about. A newer daemon may send a kind this build has no label for. */
export type AttentionKind =
  | "machine_offline"
  | "session_stuck"
  | "session_waiting"
  | "session_failed"
  | "setup_failed"
  | "gateway_unhealthy"
  | "disk_low"
  | "placement_waiting"
  | "setting_overwritten"
  | "update_refused";

/** One thing that needs a person, as `GET /api/attention` gives it. */
export interface AttentionItem {
  id: string;
  kind: AttentionKind | (string & {});
  /** The machine it is about, and that machine's name. */
  machine: string;
  machine_name: string;
  /** What happened. */
  what: string;
  /** What to do. */
  todo: string;
  /** The terminal to open to act on it. */
  terminal: string | null;
}

export const kindLabel: Record<AttentionKind, string> = {
  machine_offline: "Machine offline",
  session_stuck: "Session stuck",
  session_waiting: "Waiting for you",
  session_failed: "Session failed",
  setup_failed: "Setup failed",
  gateway_unhealthy: "Gateway unhealthy",
  disk_low: "Disk low",
  placement_waiting: "Placement waiting",
  setting_overwritten: "Setting overwritten",
  update_refused: "Update refused",
};

/** Only these clear when a person says so; every other item clears on its own. */
export const dismissable = (item: AttentionItem) => item.kind === "setting_overwritten";

/** A failed setup's terminal has already exited, so closing it from the item ends nothing that still runs. */
export const closable = (item: AttentionItem) => item.kind === "setup_failed" && item.terminal !== null;

/** What the item's own button does: dismisses it, or closes its exited setup terminal so it clears. */
export async function settle(item: AttentionItem): Promise<void> {
  if (closable(item)) return closeTerminal(item.terminal as string);
  return dismissItem(item.id);
}

export async function attentionItems(): Promise<AttentionItem[]> {
  const json = await get("/api/attention");
  return Array.isArray(json.items) ? (json.items as AttentionItem[]) : [];
}

export async function dismissItem(id: string): Promise<void> {
  const { status, json } = await post(`/api/attention/${encodeURIComponent(id)}/dismiss`);
  if (status !== 204) throw refused(status, json);
}
