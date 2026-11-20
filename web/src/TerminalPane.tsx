import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { useEffect, useRef, useState } from "react";
import { Backoff, socketUrl } from "./socket";

/** How long a pane tries a terminal it has never attached before it says the terminal is not here. */
const NOT_HERE_AFTER = 10_000;
/** How long a connection must stay up before the wait between attaches drops back to its start. */
const STEADY_AFTER = 10_000;

/**
 * xterm.js attached to terminal `id` over `/api/terminals/{id}`.
 *
 * The socket ends for good on 1000 (the child exited) and 1011 (the daemon gave a reason).
 * Any other end is tried again: 1013 (the holder cut a slow client off), a drop with no verdict
 * (the daemon restarting; terminals outlive it), and a socket that never opened (the daemon
 * writes a terminal's record before its holder serves the socket, so a terminal can be listed a
 * moment before it can be attached). The wait between attempts starts at 500 ms, doubles up to
 * 5 s, and drops back only once a connection has stayed up [STEADY_AFTER], so a client the
 * holder keeps cutting off backs off too. Trying stops only once the synced state no longer lists
 * the terminal as running (`running` false; `undefined` is not known yet), so a pane opened
 * while the daemon is down attaches when it is back, however long that takes. A pane that has
 * not attached after [NOT_HERE_AFTER] says the terminal is not available here, as for a terminal
 * on another machine, and keeps trying. Each attach's first frame is the whole kept screen, so
 * the screen is reset before it is written.
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
    term.focus();

    let socket: WebSocket | undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let stopped = false;
    const backoff = new Backoff();
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
      const ws = new WebSocket(socketUrl(`/api/terminals/${encodeURIComponent(id)}`));
      socket = ws;
      ws.binaryType = "arraybuffer";
      let opened = false;
      let openedAt = 0;
      let first = true;
      ws.onopen = () => {
        opened = true;
        openedAt = Date.now();
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
        }
        term.write(new Uint8Array(event.data as ArrayBuffer));
      };
      ws.onclose = (event) => {
        if (stopped) return;
        if (opened && event.code === 1000) return setStatus("Exited");
        if (opened && event.code === 1011) return setStatus(event.reason || "The terminal ended with an error");
        if (opened && Date.now() - openedAt >= STEADY_AFTER) backoff.reset();
        missedSince ??= Date.now();
        if (attached) setStatus("Disconnected; reconnecting");
        else if (Date.now() - missedSince < NOT_HERE_AFTER) setStatus("Not attached yet; trying again");
        else setStatus("This terminal is not available here; trying again");
        again(backoff.next());
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
      {/* The fit add-on sizes the terminal to its parent's height, padding included, so the padding sits outside it. */}
      <div className="min-h-0 flex-1 p-2">
        <div ref={host} className="h-full" />
      </div>
      {status && <div className="border-t border-neutral-800 px-3 py-1 text-xs text-amber-400">{status}</div>}
    </div>
  );
}
