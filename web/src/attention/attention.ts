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
  | "update_refused"
  | "run_stuck"
  | "run_needs_info"
  | "factory_idle"
  | "foreign_claim"
  | "gate_infra";

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
  /** The GitHub issue of the run it is about. */
  issue_url: string | null;
  /** The pull request the run reported. */
  pr_url: string | null;
}

/** Each kind's name for one item, and for several after their count. */
const kindName: Record<AttentionKind, [one: string, several: string]> = {
  machine_offline: ["Machine offline", "machines offline"],
  session_stuck: ["Session stuck", "sessions stuck"],
  session_waiting: ["Waiting for you", "sessions waiting for you"],
  session_failed: ["Session failed", "sessions failed"],
  setup_failed: ["Setup failed", "setups failed"],
  gateway_unhealthy: ["Gateway unhealthy", "gateways unhealthy"],
  disk_low: ["Disk low", "machines low on disk"],
  placement_waiting: ["Placement waiting", "placements waiting"],
  setting_overwritten: ["Setting overwritten", "settings overwritten"],
  update_refused: ["Update refused", "updates refused"],
  run_stuck: ["Run stuck", "runs stuck"],
  run_needs_info: ["Run needs info", "runs need info"],
  factory_idle: ["Factory idle", "factories idle"],
  foreign_claim: ["Foreign claim", "foreign claims"],
  gate_infra: ["Gate could not run", "gates could not run"],
};

/** A card's heading for `count` items of `kind`: "Run stuck", "16 runs stuck", or the raw kind when this build has no name for it. */
export function heading(kind: string, count: number): string {
  const name = kindName[kind as AttentionKind] as [string, string] | undefined;
  if (count === 1) return name?.[0] ?? kind;
  return `${count} ${name?.[1] ?? kind}`;
}

/** The items of one kind that say the same thing to do, as one card. */
export interface AttentionGroup {
  /** Its kind and todo as one string, apart for every pair: the card's React key. */
  key: string;
  kind: string;
  items: AttentionItem[];
  /** What to do, which every item of the group says. Never null from `byTodo`; nullable so a card can still say each item's own. */
  todo: string | null;
  /** The machine every item is on; null when they are on different machines. */
  machine_name: string | null;
}

/**
 * The items grouped by kind and todo together, in the order each group first comes. Two items of
 * a kind can say different things to do (a run stuck at its time limit and one stuck on a failed
 * gate), so they share a card only when their todo is the same too.
 */
export function byTodo(items: AttentionItem[]): AttentionGroup[] {
  const groups = new Map<string, AttentionGroup>();
  for (const item of items) {
    const key = JSON.stringify([item.kind, item.todo]);
    const group = groups.get(key);
    if (group) group.items.push(item);
    else groups.set(key, { key, kind: item.kind, items: [item], todo: item.todo, machine_name: null });
  }
  return [...groups.values()].map((group) => ({ ...group, machine_name: shared(group.items.map((item) => item.machine_name)) }));
}

const shared = (values: string[]) => (values.every((value) => value === values[0]) ? values[0] : null);

/** The text up to its first line break. */
export const firstLine = (text: string) => text.split("\n", 1)[0];

/**
 * A session's question, stall or failure. The page already leads to each from its state (the strip
 * at its head, the board's needs-you lane, the sidebar), so the card leaves these out.
 */
export const aboutASession = (item: AttentionItem) => item.kind === "session_waiting" || item.kind === "session_stuck" || item.kind === "session_failed";

/** Only these clear when a person says so; every other item clears on its own. */
export const dismissable = (item: AttentionItem) => item.kind === "setting_overwritten";

/**
 * A failed setup's terminal has already exited, so closing it from the item ends nothing that still runs.
 * It closes without asking, as Clear does for any ended terminal, and its final screen goes with it.
 */
export const closable = (item: AttentionItem): item is AttentionItem & { terminal: string } =>
  item.kind === "setup_failed" && item.terminal !== null;

/** What the item's own button does: dismisses it, or closes its exited setup terminal so it clears. */
export async function settle(item: AttentionItem): Promise<void> {
  if (closable(item)) return closeTerminal(item.terminal);
  return dismissItem(item.id);
}

/** What needs a person and is not a session's (`aboutASession`). */
export async function attentionItems(): Promise<AttentionItem[]> {
  const json = await get("/api/attention");
  return Array.isArray(json.items) ? (json.items as AttentionItem[]).filter((item) => !aboutASession(item)) : [];
}

export async function dismissItem(id: string): Promise<void> {
  const { status, json } = await post(`/api/attention/${encodeURIComponent(id)}/dismiss`);
  if (status !== 204) throw refused(status, json);
}
