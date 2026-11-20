import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { sessionBriefs, type AnyRecord, type Briefs, type SessionState } from "./api";

/** How often briefs are read while a session works, so its "now" line keeps up with its tools. */
export const BRIEFS_EVERY = 2000;

const busy: SessionState[] = ["starting", "working", "waiting_for_you", "stuck"];
const none: Briefs = {};

/**
 * What each of this machine's sessions is for and doing. Read once, again whenever a session
 * changes state, and every `BRIEFS_EVERY` while one is busy; never on a timer when all are quiet.
 */
export function useBriefs(records: AnyRecord[]): Briefs {
  const sessions = records.flatMap((r) => (r.kind === "session" && !r.deleted ? [r.body] : []));
  const states = sessions.map((s) => `${s.id}:${s.state}`).join(",");
  const working = sessions.some((s) => busy.includes(s.state));
  const { data } = useQuery({
    queryKey: ["briefs", states],
    queryFn: sessionBriefs,
    enabled: sessions.length > 0,
    placeholderData: keepPreviousData,
    refetchInterval: (q) => (working && q.state.status !== "error" ? BRIEFS_EVERY : false),
    retry: false,
  });
  return data ?? none;
}
