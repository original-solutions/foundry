/** The workspaces folded in the sidebar, by id, kept in this browser only. */
const KEY = "foundry.folded";

/** The folded workspaces, or none when storage is empty, blocked or holds something else. */
export function loadFolded(): string[] {
  try {
    const kept: unknown = JSON.parse(localStorage.getItem(KEY) ?? "[]");
    return Array.isArray(kept) ? kept.filter((w): w is string => typeof w === "string") : [];
  } catch {
    return [];
  }
}

export function saveFolded(folded: string[]) {
  try {
    localStorage.setItem(KEY, JSON.stringify(folded));
  } catch {
    // Storage blocked: the folds last for this page only.
  }
}
