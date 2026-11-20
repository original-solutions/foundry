import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { useEffect, useRef, useState } from "react";

/** How long a pane tries a terminal it has never attached before it says the terminal is not here. */
const GIVE_UP_AFTER = 10_000;

/**
 * xterm.js attached to terminal `id` over `/api/terminals/{id}`.
 *
 * The socket ends for good on 1000 (the child exited) and 1011 (the daemon gave a reason).
 * Any other end is tried again: 1013 (the holder cut a slow client off), a drop with no verdict
 * (the daemon restarting; terminals outlive it), and a socket that never opened (the daemon
 * writes a terminal's record before its holder serves the socket, so a terminal can be listed a
 * moment before it can be attached). Trying stops once the synced state no longer lists the
 * terminal as running (`running` false; `undefined` is not known yet), and after
 * [GIVE_UP_AFTER] when the pane has never attached it, as for a terminal on another machine.
 * Each attach's first frame is the whole kept screen, so the screen is reset before it is
 * written.
 */
export function TerminalPane({ id, running }: { id: string; running: boolean | undefined }) {
  const host = useRef<HTMLDivElement>(null);
  const [status, setStatus] = useState<string | null>(null);
  const runningNow = useRef(running);
  useEffect(() => {
    runningNow.current = running;
  }, [running]);

  useEffect(() => {
    const term = new Terminal({ cursorBlink: true, fontSize: 13, fontFamily: "ui-monospace, Menlo, monospace" });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host.current!);
    fit.fit();

    let socket: WebSocket | undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let stopped = false;
    let delay = 500;
    let attached = false;
    let missedSince: number | undefined;
    const encoder = new TextEncoder();
    const sendSize = () => {
      if (socket?.readyState === WebSocket.OPEN) socket.send(JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }));
    };
    /** True, with the status saying so, once the synced state says the terminal is not running. */
    const gone = () => {
      if (runningNow.current !== false) return false;
      setStatus("This terminal is no longer running");
      return true;
    };
    const again = (wait: number) => {
      timer = setTimeout(() => !stopped && !gone() && attach(), wait);
    };

    const attach = () => {
      const scheme = location.protocol === "https:" ? "wss" : "ws";
      const ws = new WebSocket(`${scheme}://${location.host}/api/terminals/${encodeURIComponent(id)}`);
      socket = ws;
      ws.binaryType = "arraybuffer";
      let opened = false;
      let first = true;
      ws.onopen = () => {
        opened = true;
        attached = true;
        setStatus(null);
        sendSize();
      };
      ws.onmessage = (event) => {
        if (typeof event.data === "string") {
          try {
            const message = JSON.parse(event.data) as { type?: string; error?: string };
            if (message.type === "error") setStatus(message.error ?? "error");
          } catch {
            // Not a control frame.
          }
          return;
        }
        if (first) {
          term.reset();
          first = false;
          delay = 500;
        }
        term.write(new Uint8Array(event.data as ArrayBuffer));
      };
      ws.onclose = (event) => {
        if (stopped) return;
        if (opened && event.code === 1000) return setStatus("Exited");
        if (opened && event.code === 1011) return setStatus(event.reason || "The terminal ended with an error");
        if (gone()) return;
        if (opened && event.code === 1013) return again(200);
        if (!attached) {
          missedSince ??= Date.now();
          if (Date.now() - missedSince >= GIVE_UP_AFTER) return setStatus("This terminal is not available here");
        }
        setStatus(attached ? "Disconnected; reconnecting" : "Not attached yet; trying again");
        again(delay);
        delay = Math.min(delay * 2, 5000);
      };
    };

    const typed = term.onData((data) => {
      if (socket?.readyState === WebSocket.OPEN) socket.send(encoder.encode(data));
    });
    const resized = term.onResize(sendSize);
    const observer = new ResizeObserver(() => fit.fit());
    observer.observe(host.current!);
    attach();

    return () => {
      stopped = true;
      clearTimeout(timer);
      observer.disconnect();
      typed.dispose();
      resized.dispose();
      socket?.close();
      term.dispose();
      setStatus(null);
    };
  }, [id]);

  return (
    <div className="flex h-full flex-col bg-black">
      <div ref={host} className="min-h-0 flex-1 p-2" />
      {status && <div className="border-t border-neutral-800 px-3 py-1 text-xs text-amber-400">{status}</div>}
    </div>
  );
}
