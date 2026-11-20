import type { MachineView } from "./fleet";
import { sessionLabel, sessionGlyph, terminalLabel, terminalGlyph, type Glyph } from "./labels";

/** Somewhere the jump palette goes: an open terminal, a new shell in a workspace, or a new workspace. */
export type Destination =
  | { kind: "terminal"; key: string; terminal: string; title: string; label: string; glyph: Glyph; machine: string; words: string }
  | { kind: "shell"; key: string; workspace: string; title: string; machine: string; words: string }
  | { kind: "workspace"; key: string; name: string; title: string; machine: string; words: string };

/** Every session and shell in the fleet in sidebar order, then a new shell for each workspace. */
export function destinations(machines: MachineView[]): Destination[] {
  const open: Destination[] = [];
  const shells: Destination[] = [];
  for (const machine of machines) {
    for (const w of machine.workspaces) {
      const where = `${w.name} ${w.project} ${w.branch} ${machine.name}`;
      const add = (terminal: string, name: string, label: string, glyph: Glyph) =>
        open.push({ kind: "terminal", key: terminal, terminal, title: `${w.name} / ${name}`, label, glyph, machine: machine.name, words: `${where} ${name} ${label}` });
      for (const s of w.sessions) add(s.terminal, s.model, sessionLabel[s.state], sessionGlyph[s.state]);
      for (const t of w.shells) add(t.id, "shell", terminalLabel(t.state, t.exitCode), terminalGlyph(t.state, t.exitCode));
      const title = `New shell in ${w.name}`;
      shells.push({ kind: "shell", key: `new-shell:${w.id}`, workspace: w.id, title, machine: machine.name, words: `${title} ${where}` });
    }
  }
  return [...open, ...shells];
}

/** The destinations holding every word of `query`, ignoring case. */
export function search(all: Destination[], query: string): Destination[] {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  return all.filter((d) => {
    const hay = d.words.toLowerCase();
    return words.every((word) => hay.includes(word));
  });
}

/** A new workspace named with what was typed, or none when nothing was. */
export function newWorkspace(query: string): Destination[] {
  const name = query.trim().replace(/\s+/g, " ");
  return name ? [{ kind: "workspace", key: "new-workspace", name, title: `New workspace “${name}”`, machine: "", words: "" }] : [];
}
