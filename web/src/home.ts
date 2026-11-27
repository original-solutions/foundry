import { useSyncExternalStore } from "react";
import { readStored, store } from "./storage";

/** Which of the board and the timeline the home shows. The machines and factory views have their own URLs. */
export type Home = "board" | "timeline";

const KEY = "foundry.home";
/** The pick of this page when the browser keeps nothing. */
let unkept: Home | undefined;
const listeners = new Set<() => void>();

function current(): Home {
  const kept = readStored(KEY);
  if (kept === "board" || kept === "timeline") return kept;
  return unkept ?? "board";
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** The home's view, kept in this browser, which the board's switch and the jump palette both change. */
export function useHome(): Home {
  return useSyncExternalStore(subscribe, current);
}

/** Shows `home` wherever the home is shown, and keeps it for next time; when not kept, it holds for this page. */
export function setHome(home: Home) {
  unkept = store(KEY, home) ? undefined : home;
  for (const listener of listeners) listener();
}
