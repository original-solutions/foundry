import { useLayoutEffect, useRef } from "react";

/**
 * Whether `event` is the app's shortcut for `key`: Cmd+key anywhere, or Ctrl+key outside the
 * terminal, where the shell keeps Ctrl for itself. Not with Alt or Shift.
 */
export function isShortcut(event: KeyboardEvent, key: string): boolean {
  if (event.key.toLowerCase() !== key || event.altKey || event.shiftKey) return false;
  const inTerminal = event.target instanceof Element && event.target.closest(".xterm");
  return event.metaKey || (event.ctrlKey && !inTerminal);
}

/** Runs `run` on the shortcut for `key` (see `isShortcut`), ahead of whatever has focus. No `run`, no shortcut. */
export function useShortcut(key: string, run: (() => void) | undefined) {
  const latest = useRef(run);
  useLayoutEffect(() => {
    latest.current = run;
  });
  const on = run !== undefined;
  useLayoutEffect(() => {
    if (!on) return;
    const listener = (event: KeyboardEvent) => {
      if (!isShortcut(event, key)) return;
      event.preventDefault();
      event.stopPropagation();
      latest.current?.();
    };
    window.addEventListener("keydown", listener, true);
    return () => window.removeEventListener("keydown", listener, true);
  }, [key, on]);
}

/** The keys that open Settings, as the palette and the sheet name them. */
export const settingsKey = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent) ? "⌘," : "Ctrl ,";

/** Whether a key typed at `target` is text for a field or a terminal, not a key for the app. */
export function typing(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  return target.isContentEditable || target.closest("input, textarea, select, .xterm") !== null;
}

/**
 * Runs `run` on Cmd+, (Ctrl+, off macOS), unless it is typed into a field or a terminal, where a
 * comma is text. Not with Alt or Shift.
 */
export function useSettingsKey(run: () => void) {
  const latest = useRef(run);
  useLayoutEffect(() => {
    latest.current = run;
  });
  useLayoutEffect(() => {
    const listener = (event: KeyboardEvent) => {
      if (event.key !== "," || event.altKey || event.shiftKey || !(event.metaKey || event.ctrlKey) || typing(event.target)) return;
      event.preventDefault();
      latest.current();
    };
    window.addEventListener("keydown", listener);
    return () => window.removeEventListener("keydown", listener);
  }, []);
}
