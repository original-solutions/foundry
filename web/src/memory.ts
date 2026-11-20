import { useEffect, useRef, useState } from "react";
import type { AnyRecord, SharedSettings } from "./api";
import type { FleetState } from "./fleet";

/** Where this browser keeps the fleet the daemon last sent. */
export const MEMORY_KEY = "foundry.last";
/** The least time between two keeps, so a burst of records is written once. */
export const KEEP_AFTER = 1000;
/** The shape kept; another is not read. */
const VERSION = 1;

/** The fleet as the daemon last sent it to this browser, and when. */
export interface Remembered {
  at: number;
  records: AnyRecord[];
  settings: SharedSettings;
}

/**
 * Keeps `state` as this browser's last sight of the fleet. Only a full state: before the first one
 * an empty list does not mean an empty fleet. It holds what the page already shows (records and
 * settings), never a terminal's bytes or a brief.
 */
export function keep(state: FleetState, at: number) {
  if (!state.synced) return;
  try {
    localStorage.setItem(MEMORY_KEY, JSON.stringify({ v: VERSION, at, records: state.records, settings: state.settings }));
  } catch {
    // Storage blocked or full: the next load waits for the daemon, as it did before.
  }
}

/** The fleet this browser last saw, or nothing when none was kept or it is not one this page reads. */
export function recall(): Remembered | undefined {
  try {
    const kept = JSON.parse(localStorage.getItem(MEMORY_KEY) ?? "null") as { v?: unknown; at?: unknown; records?: unknown; settings?: unknown } | null;
    if (kept?.v !== VERSION || typeof kept.at !== "number" || !Array.isArray(kept.records) || typeof kept.settings !== "object" || !kept.settings) return undefined;
    return { at: kept.at, records: kept.records as AnyRecord[], settings: kept.settings as SharedSettings };
  } catch {
    return undefined;
  }
}

/**
 * The fleet this browser last saw, read once at load; and the live `state` kept for the next load
 * at most once every [KEEP_AFTER], however fast records arrive. A frozen one (`live` false, the
 * link lost) is never kept: it would be dated as if just seen.
 */
export function useMemory(state: FleetState, live: boolean): Remembered | undefined {
  const [memory] = useState(recall);
  const latest = useRef({ state, live });
  const pending = useRef<ReturnType<typeof setTimeout>>(undefined);
  useEffect(() => {
    latest.current = { state, live };
    if (!state.synced || !live || pending.current) return;
    pending.current = setTimeout(() => {
      pending.current = undefined;
      if (latest.current.live) keep(latest.current.state, Date.now());
    }, KEEP_AFTER);
  }, [state, live]);
  useEffect(
    () => () => {
      clearTimeout(pending.current);
      pending.current = undefined;
    },
    [],
  );
  return memory;
}
