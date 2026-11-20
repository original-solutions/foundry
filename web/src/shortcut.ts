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
