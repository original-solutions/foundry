import type { Alert } from "./alerts";
import { rows, type MachineView, type Row } from "./fleet";

/** How long a finished turn's card stays before it goes by itself; one that needs you stays. */
export const TOAST_MS = 12_000;
/** The most cards shown at once: the newest. */
export const MAX_TOASTS = 3;

/** A card for an alert raised while the page is in front: what it says, and the glyph it came with. */
export type Toast = Alert;

/**
 * The cards after `alerts` arrive: each one for a terminal not on screen goes on top and replaces
 * its session's older card; only the newest `MAX_TOASTS` stay.
 */
export function arrive(list: Toast[], alerts: Alert[], onScreen: string[]): Toast[] {
  const fresh = alerts.filter((a) => !onScreen.includes(a.terminal)).reverse();
  if (fresh.length === 0) return list;
  const tags = new Set(fresh.map((a) => a.tag));
  return [...fresh, ...list.filter((t) => !tags.has(t.tag))].slice(0, MAX_TOASTS);
}

/** A card's session as it is now: its row, its workspace's name and its machine. */
export interface Live {
  toast: Toast;
  row: Row;
  machine: string;
}

/**
 * The cards still worth showing: a card lasts while its session keeps the state it came with and
 * its terminal is not on screen, so answering it, or opening it some other way, takes it off.
 */
export function current(list: Toast[], machines: MachineView[], onScreen: string[]): Live[] {
  return list.flatMap((toast) => {
    if (onScreen.includes(toast.terminal)) return [];
    for (const machine of machines) {
      for (const workspace of machine.workspaces) {
        const row = rows(workspace).find((r) => r.session === toast.tag);
        if (row) return row.glyph === toast.glyph ? [{ toast, row, machine: machine.id }] : [];
      }
    }
    return [];
  });
}
