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

export function saveTheme(theme: Theme) {
  // When not kept, the choice holds until the page reloads.
  store(KEY, theme);
}
