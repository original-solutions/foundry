import { useEffect, useState } from "react";
import type { UiMessage } from "./api";
import { apply, emptyState } from "./fleet";

/** The synced state from `/api/events`, reconnecting with a short backoff after a close. */
export function useFleet() {
  const [state, setState] = useState(emptyState);
  const [connected, setConnected] = useState(false);

  useEffect(() => {
    let stopped = false;
    let socket: WebSocket | undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let delay = 500;

    const open = () => {
      const scheme = location.protocol === "https:" ? "wss" : "ws";
      socket = new WebSocket(`${scheme}://${location.host}/api/events`);
      socket.onopen = () => setConnected(true);
      socket.onmessage = (event) => {
        // The backoff resets only once the daemon has sent something, so a socket that opens and
        // fails at once is not retried every 500 ms.
        delay = 500;
        try {
          const message = JSON.parse(String(event.data)) as UiMessage;
          setState((current) => apply(current, message));
        } catch {
          // A frame that is not a message is skipped.
        }
      };
      socket.onclose = () => {
        setConnected(false);
        if (stopped) return;
        timer = setTimeout(open, delay);
        delay = Math.min(delay * 2, 5000);
      };
    };
    open();

    return () => {
      stopped = true;
      clearTimeout(timer);
      socket?.close();
    };
  }, []);

  return { state, connected };
}
