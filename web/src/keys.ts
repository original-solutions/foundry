import type { Glyph } from "./labels";

/** One key on the touch key bar: what it shows, what it says to a screen reader, and what it sends. */
export interface Key {
  label: string;
  title: string;
  /** The bytes it sends, or an arrow, whose bytes depend on the program's cursor-key mode. */
  data: string | { arrow: "A" | "B" | "C" | "D" };
  /** One of Claude Code's numbered answers, offered while the session waits for you. */
  answer?: boolean;
}

const always: Key[] = [
  { label: "esc", title: "Escape", data: "\x1b" },
  { label: "⇧tab", title: "Shift+Tab", data: "\x1b[Z" },
  { label: "tab", title: "Tab", data: "\t" },
  { label: "↑", title: "Up", data: { arrow: "A" } },
  { label: "↓", title: "Down", data: { arrow: "B" } },
  { label: "←", title: "Left", data: { arrow: "D" } },
  { label: "→", title: "Right", data: { arrow: "C" } },
  { label: "^C", title: "Ctrl+C", data: "\x03" },
  { label: "⏎", title: "Enter", data: "\r" },
];

const answers: Key[] = ["1", "2", "3"].map((n) => ({ label: n, title: `Answer ${n}`, data: n, answer: true }));

/**
 * The keys a phone's keyboard lacks, for a terminal in state `glyph`. A session waiting for you
 * (known from its hooks, not from its screen) gets Claude Code's numbered answers first, so a
 * permission prompt takes one tap.
 */
export function keysFor(glyph: Glyph): Key[] {
  return glyph === "waiting" ? [...answers, ...always] : always;
}

/** The bytes `key` sends to a terminal in `modes`: arrows follow the program's cursor-key mode, as a keyboard's do. */
export function keyData(key: Key, modes: { applicationCursorKeysMode: boolean }): string {
  if (typeof key.data === "string") return key.data;
  return `\x1b${modes.applicationCursorKeysMode ? "O" : "["}${key.data.arrow}`;
}
