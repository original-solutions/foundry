/** The open tabs, terminal ids in strip order, kept in this browser only. */
const KEY = "foundry.tabs";

/** `tabs` with terminal `id` added at the end, or `tabs` itself when it is already there. */
export function withTab(tabs: string[], id: string): string[] {
  return tabs.includes(id) ? tabs : [...tabs, id];
}

/** `tabs` without `id`, and the tab to show in its place: its right neighbour, else its left. */
export function withoutTab(tabs: string[], id: string): { tabs: string[]; next: string | undefined } {
  const at = tabs.indexOf(id);
  const rest = tabs.filter((t) => t !== id);
  return { tabs: rest, next: rest[Math.min(at, rest.length - 1)] };
}

/** The tabs this browser had open, or none when storage is empty, blocked or holds something else. */
export function loadTabs(): string[] {
  try {
    const kept: unknown = JSON.parse(localStorage.getItem(KEY) ?? "[]");
    return Array.isArray(kept) ? kept.filter((t): t is string => typeof t === "string") : [];
  } catch {
    return [];
  }
}

export function saveTabs(tabs: string[]) {
  try {
    localStorage.setItem(KEY, JSON.stringify(tabs));
  } catch {
    // Storage blocked: the tabs last for this page only.
  }
}
