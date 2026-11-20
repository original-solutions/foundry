import { useCallback, useEffect, useRef, useState } from "react";
import type { UiMessage } from "./api";
import { apply, emptyState } from "./fleet";
import { remember, type History } from "./samples";
import { Backoff, socketUrl } from "./socket";

/** The link to the daemon: since when it was lost, when it is tried next, and how many tries failed. */
export interface Link {
  /** When the socket that last sent something closed, or the first try failed; none while live. */
  lostAt?: number;
  /** When the next try opens; none while one is under way. */
  retryAt?: number;
  /** Closes since the daemon last sent something. */
  tries: number;
  /** Once live again after being lost: for how long it was lost, and when it came back. */
  back?: { after: number; at: number };
  /** Tries now instead of waiting out the backoff. */
  retry: () => void;
}

/**
 * The synced state from `/api/events`, reconnecting with a short backoff after a close, and each
 * machine's samples as they arrived while this page was open (up to `SAMPLES_WINDOW`), and the
 * link: lost from a close until the daemon sends something again.
 */
export function useFleet() {
  const [state, setState] = useState(emptyState);
  const [samples, setSamples] = useState<History>({});
  const [connected, setConnected] = useState(false);
  const [link, setLink] = useState<Omit<Link, "retry">>({ tries: 0 });
  const retryNow = useRef(() => {});
  const retry = useCallback(() => retryNow.current(), []);

  useEffect(() => {
    let stopped = false;
    let socket: WebSocket | undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const backoff = new Backoff();

    const open = () => {
      timer = undefined;
      socket = new WebSocket(socketUrl("/api/events"));
      socket.onopen = () => setConnected(true);
      socket.onmessage = (event) => {
        // The backoff resets only once the daemon has sent something, so a socket that opens and
        // fails at once is not retried every 500 ms.
        backoff.reset();
        setLink((current) => {
          if (current.lostAt === undefined) return current;
          const now = Date.now();
          return { tries: 0, back: { after: now - current.lostAt, at: now } };
        });
        try {
          const message = JSON.parse(String(event.data)) as UiMessage;
          setState((current) => apply(current, message));
          const records = message.type === "state" ? message.records : message.type === "record" ? [message.record] : [];
          const now = Date.now();
          setSamples((history) => remember(history, records, now));
        } catch {
          // A frame that is not a message is skipped.
        }
      };
      socket.onclose = () => {
        setConnected(false);
        if (stopped) return;
        const wait = backoff.next();
        const now = Date.now();
        setLink((current) => ({ lostAt: current.lostAt ?? now, retryAt: now + wait, tries: current.tries + 1 }));
        timer = setTimeout(open, wait);
      };
    };
    retryNow.current = () => {
      // Only while waiting: a try under way is not doubled.
      if (stopped || timer === undefined) return;
      clearTimeout(timer);
      setLink(({ lostAt, tries }) => ({ lostAt, tries }));
      open();
    };
    open();

    return () => {
      stopped = true;
      clearTimeout(timer);
      socket?.close();
    };
  }, []);

  return { state, connected, samples, link: { ...link, retry } };
}
