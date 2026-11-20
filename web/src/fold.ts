import { readStoredJson, storeJson } from "./storage";

/**
 * The workspaces unfolded in the sidebar, by id, kept in this browser only. A workspace is one
 * line of its states unless it is here: the one you are in is added, and dropped when you leave.
 */
const KEY = "foundry.unfolded";

/** The unfolded workspaces, or none when storage is empty, blocked or holds something else. */
export function loadUnfolded(): string[] {
  const kept = readStoredJson(KEY);
  return Array.isArray(kept) ? kept.filter((w): w is string => typeof w === "string") : [];
}

export function saveUnfolded(unfolded: string[]) {
  // When not kept, the unfolds last for this page only.
  storeJson(KEY, unfolded);
}
