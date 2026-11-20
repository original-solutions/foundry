import { ApiError } from "./api";

/** How long a live read waits after a failed one before it tries again. */
export const RETRY_EVERY = 10_000;

/**
 * The `refetchInterval` of a read kept fresh while `live`: every `every`, and after a failed read
 * every `RETRY_EVERY` (never faster than `every`), so one bad answer does not stop it. A 404 stops
 * it: what it reads belongs to another machine, or is gone.
 */
export function pollEvery(every: number, live: boolean) {
  return (query: { state: { status: string; error: Error | null } }): number | false => {
    const { status, error } = query.state;
    if (error instanceof ApiError && error.status === 404) return false;
    if (!live) return false;
    return status === "error" ? Math.max(every, RETRY_EVERY) : every;
  };
}
