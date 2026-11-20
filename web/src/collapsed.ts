import { readStored, store } from "./storage";

/** Whether the sidebar is collapsed to a rail, kept in this browser only. */
const KEY = "foundry.rail";

/** Below this width the full list leaves a terminal too little room, so it starts as a rail. */
export const RAIL_BELOW = 1024;

/** The choice made in this browser, or, with none, a rail on a window narrower than `RAIL_BELOW`. */
export function loadRail(width: number): boolean {
  const kept = readStored(KEY);
  if (kept === "1" || kept === "0") return kept === "1";
  return width < RAIL_BELOW;
}

export function saveRail(on: boolean) {
  // When not kept, the choice lasts for this page only.
  store(KEY, on ? "1" : "0");
}

/** The key that collapses or expands the sidebar. */
export const railKey = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent) ? "⌘B" : "Ctrl B";
