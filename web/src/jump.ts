import type { ReactNode } from "react";
import { branchOf, rows, type MachineView } from "./fleet";
import type { Glyph } from "./labels";

/** Something the palette does besides opening a terminal: going to one of the app's views, or one of its actions. */
export interface Command {
  key: string;
  title: string;
  group: "Go to" | "Do";
  /** What it shows or does, in a sentence: the preview's on a wide screen, under its name on a narrow one. */
  about: string;
  /** Other words that find it. */
  also?: string;
  /** Said at its right: "Here" for the view on screen, or how many it concerns. */
  hint?: string;
  /** The key that does the same from anywhere. */
  keys?: string;
  icon?: ReactNode;
  run: () => void;
}

/** Somewhere the jump palette goes: an open terminal, a new shell in a workspace, a new workspace, or a view or action of the app. */
export type Destination =
  | { kind: "terminal"; key: string; terminal: string; title: string; subtitle?: string; label: string; fullLabel: string; glyph: Glyph; machine: string; words: string }
  | { kind: "shell"; key: string; workspace: string; title: string; machine: string; words: string }
  | { kind: "workspace"; key: string; name: string; title: string; machine: string; words: string }
  | { kind: "command"; key: string; command: Command; title: string; machine: string; words: string };

/** Every session and shell in the fleet in sidebar order, then the app's views and actions, then a new shell for each workspace. */
export function destinations(machines: MachineView[], commands: Command[] = []): Destination[] {
  const open: Destination[] = [];
  const shells: Destination[] = [];
  for (const machine of machines) {
    for (const w of machine.workspaces) {
      const where = `${w.name} ${w.project} ${branchOf(w)} ${machine.name}`;
      // As the sidebar lists them, so a factory run's quiet session reads alike here, found by all of a held-up run's reason.
      for (const r of rows(w))
        open.push({ kind: "terminal", key: r.terminal, terminal: r.terminal, title: `${w.name} / ${r.name}`, subtitle: r.title, label: r.label, fullLabel: r.fullLabel, glyph: r.glyph, machine: machine.name, words: `${where} ${r.name} ${r.fullLabel} ${r.title ?? ""}` });
      const title = `New shell in ${w.name}`;
      shells.push({ kind: "shell", key: `new-shell:${w.id}`, workspace: w.id, title, machine: machine.name, words: `${title} ${where}` });
    }
  }
  const done: Destination[] = commands.map((c) => ({ kind: "command", key: `command:${c.key}`, command: c, title: c.title, machine: "", words: `${c.title} ${c.also ?? ""}` }));
  return [...open, ...done, ...shells];
}

/**
 * The destinations holding every word of `query`, ignoring case. Once something is typed, the
 * views and actions it found come first: there are few of them, and a word that names one is
 * meant for it more often than for a workspace that happens to hold it.
 */
export function search(all: Destination[], query: string): Destination[] {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  const found = all.filter((d) => {
    const hay = d.words.toLowerCase();
    return words.every((word) => hay.includes(word));
  });
  if (words.length === 0) return found;
  return [...found.filter((d) => d.kind === "command"), ...found.filter((d) => d.kind !== "command")];
}

/** A new task of what was typed (its workspace named from it), or none when nothing was. */
export function newWorkspace(query: string): Destination[] {
  const name = query.trim().replace(/\s+/g, " ");
  return name ? [{ kind: "workspace", key: "new-workspace", name, title: `New task “${name}”`, machine: "", words: "" }] : [];
}
