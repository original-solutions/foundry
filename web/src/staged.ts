import { useQueries } from "@tanstack/react-query";
import { useState } from "react";
import type { Shipping } from "./api";
import { changesQuery } from "./ChangesPanel";
import { totals } from "./changes";
import type { Card } from "./lanes";
import { BOARD_BUSY_EVERY, pollEvery } from "./poll";
import { CHECKS_EVERY, shippingQuery } from "./Shipping";
import { stageIndex, stageOf, stages, type Stage } from "./stage";

/** A lane of the board grouped by shipping: a stage, a workspace that changed nothing yet, or one whose work is not read from here. */
export type ShipLane = Stage | "none" | "unread";

/** Each lane's name, what it holds, and its colour: work only here is at risk, pushed is safe, in review is on its way, merged is done. */
export const shipLanes: { lane: ShipLane; name: string; hint: string; tone: string }[] = [
  ...stages.map((s) => ({ lane: s.stage, name: s.name, hint: s.hint, tone: { here: "text-amber-300", pushed: "text-sky-300", review: "text-emerald-400", merged: "text-violet-300" }[s.stage] })),
  { lane: "none", name: "No changes yet", hint: "No edits or commits since it left its base branch", tone: "text-neutral-400" },
  { lane: "unread", name: "Not read from here", hint: "Another machine's workspace, or one whose changes could not be read", tone: "text-neutral-500" },
];

const pending = (s: Shipping | undefined) => s?.pr?.state === "open" && s.pr.checks === "pending";

export interface Staged {
  /** The lanes with cards in them, in order. */
  lanes: { lane: ShipLane; cards: Card[] }[];
  /** Workspaces of this machine whose reads have not come back yet. */
  reading: number;
  /** How many workspaces have work, and its lines and commits summed. */
  sum: { workspaces: number; added: number; removed: number; commits: number };
  /** The stage each workspace moved forward to while shown, so its card lights up once on arriving. */
  arrived: Record<string, Stage>;
}

/**
 * The board's cards grouped by how far each workspace's work has gone on its way to main, while
 * `on`. Only this machine's (`here`) workspaces are read: each one's changes and branch once, again
 * when a card marks them out of date (a terminal changed state), its changes every
 * `BOARD_BUSY_EVERY` while an agent in it works and its pull request every `CHECKS_EVERY` while its
 * checks run. A workspace being archived (`leaving`) is not read again. Another machine's cards, and
 * ones that could not be read, go in a lane of their own.
 */
export function useStaged(cards: Card[], here: string | undefined, on: boolean, leaving: Set<string>): Staged {
  const mine = on && here !== undefined ? cards.filter((c) => c.machine.id === here) : [];
  const changes = useQueries({
    queries: mine.map((c) => ({
      ...changesQuery(c.workspace.id),
      enabled: !leaving.has(c.workspace.id),
      staleTime: BOARD_BUSY_EVERY,
      retry: false,
      refetchInterval: pollEvery(BOARD_BUSY_EVERY, c.rows.some((r) => r.glyph === "working")),
    })),
  });
  const shipping = useQueries({
    queries: mine.map((c) => ({
      ...shippingQuery(c.workspace.id),
      enabled: !leaving.has(c.workspace.id),
      staleTime: 30_000,
      retry: false,
      refetchInterval: (query: { state: { data: Shipping | undefined; status: string; error: Error | null } }) => pollEvery(CHECKS_EVERY, pending(query.state.data))(query),
    })),
  });

  const stageFor = new Map<string, ShipLane>();
  let reading = 0;
  const sum = { workspaces: 0, added: 0, removed: 0, commits: 0 };
  mine.forEach((c, i) => {
    const read = changes[i];
    const ship = shipping[i];
    if (read.data && !ship.isPending) {
      const stage = stageOf(read.data, ship.data);
      stageFor.set(c.workspace.id, stage);
      if (stage === "none") return;
      const t = totals(read.data);
      sum.workspaces += 1;
      sum.added += t.added;
      sum.removed += t.removed;
      sum.commits += read.data.commits.length;
    } else if (read.isError) stageFor.set(c.workspace.id, "unread");
    else reading += 1;
  });
  for (const c of cards) if (on && here !== undefined && c.machine.id !== here) stageFor.set(c.workspace.id, "unread");

  const arrived = useArrivals(stageFor);
  const lanes = shipLanes.map(({ lane }) => ({ lane, cards: cards.filter((c) => stageFor.get(c.workspace.id) === lane) })).filter((l) => l.cards.length > 0);
  return { lanes, reading, sum, arrived };
}

/**
 * The stage each workspace moved forward to since it was first placed. Kept as state and adjusted
 * while rendering, as React suggests for state that follows props; a step back (a pull request
 * closed) is not marked.
 */
function useArrivals(placed: Map<string, ShipLane>): Record<string, Stage> {
  const now = Object.fromEntries([...placed].filter(([, lane]) => lane !== "unread")) as Record<string, Stage | "none">;
  const key = JSON.stringify(now);
  const [seen, setSeen] = useState<{ key: string; stages: Record<string, Stage | "none">; arrived: Record<string, Stage> }>({ key, stages: now, arrived: {} });
  if (seen.key === key) return seen.arrived;
  const arrived = { ...seen.arrived };
  for (const [id, stage] of Object.entries(now)) {
    const was = seen.stages[id];
    if (stage === "none" || was === undefined || was === stage) continue;
    if (was === "none" || stageIndex(stage) > stageIndex(was)) arrived[id] = stage;
    else delete arrived[id];
  }
  setSeen({ key, stages: { ...seen.stages, ...now }, arrived });
  return arrived;
}
