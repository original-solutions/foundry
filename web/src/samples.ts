import type { AnyRecord } from "./api";

/** How far back the machines view draws each machine's load and memory: ten minutes, 120 samples. */
export const SAMPLES_WINDOW = 10 * 60_000;

/** One machine sample as this page received it. `at` is this browser's clock, for drawing only. */
export interface Point {
  version: number;
  at: number;
  load: number;
  memory: number;
}

export interface Trace {
  points: Point[];
  /** When a sample newer than the first one this page saw arrived: the machine was heard then. */
  heard?: number;
}

/** Each machine's recent samples, by machine id. */
export type History = Record<string, Trace>;

/**
 * Folds the machine samples in `records` into `history` at time `now`: a sample with a version
 * newer than the machine's last point becomes a point, and points older than the window go. Gives
 * back `history` itself when nothing changed. The first sample of a machine is not "heard": it
 * may be an offline machine's last word, synced long ago.
 */
export function remember(history: History, records: AnyRecord[], now: number): History {
  let next = history;
  for (const r of records) {
    if (r.kind !== "machine_sample") continue;
    const trace = next[r.owner];
    const last = trace?.points.at(-1);
    if (last && r.version <= last.version) continue;
    const point = { version: r.version, at: now, load: r.body.load_per_core, memory: r.body.free_memory };
    const points = [...(trace?.points ?? []), point].filter((p) => now - p.at <= SAMPLES_WINDOW);
    next = { ...next, [r.owner]: { points, heard: trace ? now : undefined } };
  }
  return next;
}

/** How long ago `heard` was, in a few words, or nothing when the machine has not been heard. */
export function heardAgo(heard: number | undefined, now: number): string | undefined {
  if (heard === undefined) return undefined;
  const seconds = Math.floor((now - heard) / 1000);
  if (seconds < 1) return "just now";
  if (seconds < 60) return `${seconds} s ago`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)} min ago`;
  return `${Math.floor(seconds / 3600)} h ago`;
}

/** The shortest stretch of time a line covers, so its first few points do not fill it at once. */
const SHORTEST = 60_000;

/**
 * How much time a line covers at `now`: back to its oldest point, at least a minute and at most
 * the window. A page open for two minutes draws those two minutes across the whole width.
 */
export function span(points: Point[], now: number): number {
  const oldest = points.at(0)?.at ?? now;
  return Math.min(SAMPLES_WINDOW, Math.max(SHORTEST, now - oldest));
}

/** A span in words for the line's caption: "last 3 min". */
export const spanWords = (span: number) => `last ${Math.ceil(span / 60_000)} min`;

/**
 * Points as `[x, y]` in a unit square: x by time across the `span` ending `now` (0 its start,
 * 1 now), y from the top (0 at `top` or over it, 1 at zero).
 */
export function scaled(points: Point[], value: (p: Point) => number, top: number, now: number, span: number): [number, number][] {
  return points.map((p) => [1 - (now - p.at) / span, 1 - Math.min(value(p) / top, 1)]);
}
