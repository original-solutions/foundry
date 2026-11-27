import { ApiError } from "./api";

/** How long a live read waits after a failed one before it tries again. */
export const RETRY_EVERY = 10_000;

/** Whether a read failed because the daemon answered 404: what it reads belongs to another machine, or is gone. */
export function gone(error: unknown): boolean {
  return error instanceof ApiError && error.status === 404;
}

/**
 * The `refetchInterval` of a read kept fresh while `live`: every `every`, and after a failed read
 * every `RETRY_EVERY` (never faster than `every`), so one bad answer does not stop it. A 404 stops
 * it: what it reads belongs to another machine, or is gone.
 */
export function pollEvery(every: number, live: boolean) {
  return (query: { state: { status: string; error: Error | null } }): number | false => {
    const { status, error } = query.state;
    if (gone(error)) return false;
    if (!live) return false;
    return status === "error" ? Math.max(every, RETRY_EVERY) : every;
  };
}

/**
 * How often a workspace's changes are read while an agent in it works, on a board card on screen
 * or grouped by shipping. Otherwise they are read when shown and when one of its terminals changes
 * state, never on a timer: each read runs git several times, and a fleet has dozens of workspaces.
 */
export const BOARD_BUSY_EVERY = 30_000;
