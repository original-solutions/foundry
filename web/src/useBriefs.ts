import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useMemo, useRef } from "react";
import { sessionBriefs, type AnyRecord, type Briefs, type SessionState } from "./api";
import { pollEvery } from "./poll";

/** How often briefs are read while a session works, so its "now" line keeps up with its tools. */
export const BRIEFS_EVERY = 2000;

/** States in which a session runs tools without a hook saying so in its state. */
const busy: SessionState[] = ["starting", "working"];
const none: Briefs = {};
const key = ["briefs"];

/**
 * What each of this machine's sessions is for and doing. Read once, again whenever a session
 * changes state, and every `BRIEFS_EVERY` while one works; never on a timer when all are waiting
 * or quiet. After a failed read it tries again more slowly, and shows no "now" until a read works.
 */
export function useBriefs(records: AnyRecord[]): Briefs {
  const sessions = records.flatMap((r) => (r.kind === "session" && !r.deleted ? [r.body] : []));
  const states = sessions.map((s) => `${s.id}:${s.state}`).join(",");
  const working = sessions.some((s) => busy.includes(s.state));
  const client = useQueryClient();
  const { data, isError } = useQuery({
    queryKey: key,
    queryFn: sessionBriefs,
    enabled: sessions.length > 0,
    refetchInterval: pollEvery(BRIEFS_EVERY, working),
    retry: false,
  });
  const last = useRef(states);
  useEffect(() => {
    if (last.current === states) return;
    last.current = states;
    void client.invalidateQueries({ queryKey: key, exact: true });
  }, [states, client]);
  return useMemo(() => {
    if (!data) return none;
    if (!isError) return data;
    return Object.fromEntries(Object.entries(data).map(([id, brief]) => [id, { ...brief, now: null }]));
  }, [data, isError]);
}
