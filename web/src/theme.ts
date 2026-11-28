import { useSyncExternalStore } from "react";
import { readStored, store } from "./storage";

// Dark or light. The page carries the choice as `data-theme` on <html>; index.css remaps the
// palette for light, so every screen follows without its own light classes.

export type Theme = "dark" | "light";

const KEY = "foundry.theme";

/** The shade a light screen shows in place of `shade`: the scale mirrored, 50 and 950 swapped. */
export function flippedShade(shade: number): number {
  if (shade === 50) return 950;
  if (shade === 950) return 50;
  return 1000 - shade;
}

/** The user's choice in this browser. The app is dark until they pick light. */
export function storedTheme(): Theme {
  return readStored(KEY) === "light" ? "light" : "dark";
}

export function applyTheme(theme: Theme, root: HTMLElement = document.documentElement) {
  root.dataset.theme = theme;
}

/** The pick of this page when the browser keeps nothing. */
let unkept: Theme | undefined;
const listeners = new Set<() => void>();

function current(): Theme {
  const kept = readStored(KEY);
  if (kept === "light" || kept === "dark") return kept;
  return unkept ?? "dark";
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** The theme, kept in this browser, which the sidebar's toggle and Settings → Appearance both read, so each shows the other's change at once. */
export function useTheme(): Theme {
  return useSyncExternalStore(subscribe, current);
}

/** Shows `theme` on the page and keeps it for next time; when not kept, it holds for this page. */
export function setTheme(theme: Theme) {
  unkept = store(KEY, theme) ? undefined : theme;
  applyTheme(theme);
  for (const listener of listeners) listener();
}
