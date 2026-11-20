import { ApiError } from "../api";

/** What an item is about. A newer daemon may send a kind this build has no label for. */
export type AttentionKind =
  | "machine_offline"
  | "session_stuck"
  | "session_waiting"
  | "session_failed"
  | "setup_failed"
  | "gateway_unhealthy"
  | "disk_low"
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
  setting_overwritten: "Setting overwritten",
  update_refused: "Update refused",
};

/** Only these clear when a person says so; every other item clears on its own. */
export const dismissable = (item: AttentionItem) => item.kind === "setting_overwritten";

async function answer(response: Response): Promise<Record<string, unknown>> {
  let json: Record<string, unknown> = {};
  try {
    const parsed: unknown = JSON.parse(await response.text());
    if (parsed && typeof parsed === "object") json = parsed as Record<string, unknown>;
  } catch {
    // An empty or non-JSON body, as a 204 has.
  }
  if (!response.ok) {
    throw new ApiError(typeof json.error === "string" ? json.error : `request failed (${response.status})`, response.status);
  }
  return json;
}

export async function attentionItems(): Promise<AttentionItem[]> {
  const json = await answer(await fetch("/api/attention"));
  return Array.isArray(json.items) ? (json.items as AttentionItem[]) : [];
}

export async function dismissItem(id: string): Promise<void> {
  await answer(await fetch(`/api/attention/${encodeURIComponent(id)}/dismiss`, { method: "POST" }));
}
