import type { Changes } from "./api";
import { byUrgency, rows, type MachineView, type Row, type WorkspaceView } from "./fleet";
import { addsSteps, needsYou, urgency, type Glyph } from "./labels";

/** Where a workspace sits on the board: it needs a person, it is busy, or nothing is happening. */
export type Lane = "needs" | "working" | "quiet";

export interface Card {
  workspace: WorkspaceView;
  machine: MachineView;
  /** Its terminals, most urgent first. */
  rows: Row[];
  /** The terminal a click on the card opens: its most urgent one. */
  lead?: Row;
}

const laneOf = (glyph: Glyph | undefined): Lane => (glyph === undefined ? "quiet" : needsYou.includes(glyph) ? "needs" : addsSteps.includes(glyph) ? "working" : "quiet");

const rank = (card: Card) => (card.lead ? urgency.indexOf(card.lead.glyph) : urgency.length);

/**
 * Every workspace in the fleet as a card, in lanes by its most urgent terminal: the ones that need
 * a person, the ones working, and the quiet rest. Within a lane the most urgent comes first; ties
 * keep the sidebar's order. Empty lanes are left out.
 */
export function lanes(machines: MachineView[]): { lane: Lane; cards: Card[] }[] {
  const cards = machines.flatMap((machine) =>
    machine.workspaces.map((workspace) => {
      const sorted = byUrgency(rows(workspace));
      return { workspace, machine, rows: sorted, lead: sorted[0] };
    }),
  );
  return (["needs", "working", "quiet"] as const)
    .map((lane) => ({ lane, cards: cards.filter((c) => laneOf(c.lead?.glyph) === lane).sort((a, b) => rank(a) - rank(b)) }))
    .filter((l) => l.cards.length > 0);
}

/**
 * The board's lanes in one line, for a glance from a phone: how many workspaces need you (said
 * plainly when none do), then how many work and rest. A lane with nothing in it is left out.
 */
export function glance(board: { lane: Lane; cards: Card[] }[]): string {
  const count = (lane: Lane) => board.find((l) => l.lane === lane)?.cards.length ?? 0;
  const needs = count("needs");
  return [
    needs === 0 ? "Nothing needs you" : `${needs} ${needs === 1 ? "needs" : "need"} you`,
    ...(count("working") ? [`${count("working")} working`] : []),
    ...(count("quiet") ? [`${count("quiet")} quiet`] : []),
  ].join(" · ");
}

/** The `count` files with the most lines changed, and how many others there are. */
export function topFiles(changes: Changes, count: number): { names: string[]; more: number } {
  const size = (f: Changes["files"][number]) => (f.added ?? 0) + (f.removed ?? 0);
  const names = [...changes.files]
    .sort((a, b) => size(b) - size(a))
    .slice(0, count)
    .map((f) => f.path);
  return { names, more: changes.files.length - names.length };
}
