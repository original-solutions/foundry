import type { Changes, FactoryStatus, HeldUp } from "./api";
import { issueUrl, parseRun } from "./factory";
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
  /** The factory run it belongs to, when a believed status names it. */
  factory?: FactoryRun;
}

/** The factory run a workspace belongs to: its id, its issue where the id names one, and whether it waits on a person. */
export interface FactoryRun {
  run: string;
  issue?: number;
  url?: string;
  held?: HeldUp["kind"];
}

/**
 * What a factory session P1 marks stuck says instead: it is quiet, for at least P1's stuck
 * threshold. A fixed text, so it reads the same on every machine and after a reload.
 */
export const QUIET = "Quiet 10+ min";

/**
 * The factory run each workspace belongs to, as the believed statuses name it: a live run's
 * workspaces, and a held-up run's. A status names only workspaces of its own project.
 */
export function factoryRuns(statuses: Record<string, FactoryStatus> | undefined): (workspace: WorkspaceView) => FactoryRun | undefined {
  const named = new Map<string, FactoryRun>();
  for (const status of Object.values(statuses ?? {})) {
    const runs: { run: string; workspaces: string[]; held?: HeldUp["kind"] }[] = [
      ...status.runs.map((r) => ({ run: r.run, workspaces: r.workspaces })),
      ...status.held_up.map((h) => ({ run: h.run, workspaces: h.workspaces ?? [], held: h.kind })),
    ];
    for (const { run, workspaces, held } of runs) {
      const parts = parseRun(run);
      const mark: FactoryRun = { run, ...(parts && { issue: parts.issue, url: issueUrl(parts) }), ...(held && { held }) };
      for (const workspace of workspaces) named.set(`${status.project}/${workspace}`, mark);
    }
  }
  return (workspace) => named.get(`${workspace.projectId}/${workspace.id}`);
}

/** A factory run's terminals as they show: a session P1 marks stuck is quiet, since the factory watches it. */
export function factoryRows(list: Row[]): Row[] {
  return list.map((row) => (row.session && row.glyph === "stuck" ? { ...row, label: QUIET } : row));
}

const laneOf = (glyph: Glyph | undefined): Lane => (glyph === undefined ? "quiet" : needsYou.includes(glyph) ? "needs" : addsSteps.includes(glyph) ? "working" : "quiet");

/**
 * A factory run's lane: only the run's own state puts it under needs you, when it is stuck or
 * needs info. A live run whose terminals would need a person works, since the factory tends them.
 */
const factoryLane = (run: FactoryRun, glyph: Glyph | undefined): Lane => {
  if (run.held) return "needs";
  const lane = laneOf(glyph);
  return lane === "needs" ? "working" : lane;
};

const rank = (card: Card) => (card.lead ? urgency.indexOf(card.lead.glyph) : urgency.length);

/**
 * Every workspace in the fleet as a card, in lanes by its most urgent terminal: the ones that need
 * a person, the ones working, and the quiet rest. A workspace a believed status in `statuses`
 * names is a factory run's, and its run decides whether it needs you (`factoryLane`). Within a
 * lane the most urgent comes first; ties keep the sidebar's order. Empty lanes are left out.
 */
export function lanes(machines: MachineView[], statuses?: Record<string, FactoryStatus>): { lane: Lane; cards: Card[] }[] {
  const factoryOf = factoryRuns(statuses);
  const cards = machines.flatMap((machine) =>
    machine.workspaces.map((workspace) => {
      const factory = factoryOf(workspace);
      const sorted = byUrgency(factory ? factoryRows(rows(workspace)) : rows(workspace));
      const card: Card = { workspace, machine, rows: sorted, lead: sorted[0], ...(factory && { factory }) };
      return { card, lane: factory ? factoryLane(factory, card.lead?.glyph) : laneOf(card.lead?.glyph) };
    }),
  );
  return (["needs", "working", "quiet"] as const)
    .map((lane) => ({ lane, cards: cards.filter((c) => c.lane === lane).map((c) => c.card).sort((a, b) => rank(a) - rank(b)) }))
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
