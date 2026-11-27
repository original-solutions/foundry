import type { Step } from "./api";
import type { MachineView } from "./fleet";
import { needsYou, type Glyph } from "./labels";
import { lanes } from "./lanes";

/** A terminal that needs a person: where it is, what it is called, and what it asks. */
export interface Needing {
  terminal: string;
  /** Its workspace's name. */
  workspace: string;
  name: string;
  glyph: Glyph;
  label: string;
  /** What its session asks, when it asks something. */
  now?: Step;
}

/**
 * Every terminal in the fleet that needs a person (waiting, stuck or failed), in the order the
 * board shows them: workspace by workspace down its needs-you lane, most urgent first.
 */
export function needing(machines: MachineView[]): Needing[] {
  const needs = lanes(machines).find((l) => l.lane === "needs")?.cards ?? [];
  return needs.flatMap((card) =>
    card.rows
      .filter((row) => needsYou.includes(row.glyph))
      .map((row) => ({ terminal: row.terminal, workspace: card.workspace.name, name: row.name, glyph: row.glyph, label: row.label, now: row.now })),
  );
}

/**
 * The terminal to go to next from `current` (the open terminal, or none on the board): the one
 * after it among those that need you, round again to the first after the last, or the first when
 * `current` needs nothing. `others` counts those that need you besides `current`. Nothing when no
 * other terminal needs you.
 */
export function upNext(machines: MachineView[], current?: string): { next: Needing; others: number } | undefined {
  const all = needing(machines);
  const at = all.findIndex((n) => n.terminal === current);
  const others = all.length - (at === -1 ? 0 : 1);
  if (others === 0) return undefined;
  return { next: all[(at + 1) % all.length], others };
}
