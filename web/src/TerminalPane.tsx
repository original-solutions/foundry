import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { useEffect, useRef, useState } from "react";

/** xterm.js attached to terminal `id` over `/api/terminals/{id}`. */
export function TerminalPane({ id }: { id: string }) {
  const host = useRef<HTMLDivElement>(null);
  const [status, setStatus] = useState<string | null>(null);

  useEffect(() => {
    const term = new Terminal({ cursorBlink: true, fontSize: 13, fontFamily: "ui-monospace, Menlo, monospace" });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host.current!);
    fit.fit();

    let socket: WebSocket | undefined;
    let stopped = false;
    const encoder = new TextEncoder();
    const sendSize = () => {
      if (socket?.readyState === WebSocket.OPEN) socket.send(JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }));
    };

    const attach = () => {
      const scheme = location.protocol === "https:" ? "wss" : "ws";
      const ws = new WebSocket(`${scheme}://${location.host}/api/terminals/${id}`);
      socket = ws;
      ws.binaryType = "arraybuffer";
      let opened = false;
      let first = true;
      ws.onopen = () => {
        opened = true;
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
        // The first frame is the kept screen, so it replaces what is shown.
        if (first) {
          term.reset();
          first = false;
        }
        term.write(new Uint8Array(event.data as ArrayBuffer));
      };
      ws.onclose = (event) => {
        if (stopped) return;
        if (!opened) setStatus("terminal not available here");
        else if (event.code === 1000) setStatus("exited");
        else if (event.code === 1013) setTimeout(() => !stopped && attach(), 200);
        else setStatus(event.reason || `disconnected (${event.code})`);
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
