import { useQueryClient } from "@tanstack/react-query";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { Terminal, type ILink } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { memo, useCallback, useEffect, useRef, useState } from "react";
import { MAX_FILE, sendFile, type FileChange } from "./api";
import { changesQuery } from "./ChangesPanel";
import { FindBar } from "./FindBar";
import { DropOverlay, SendingChip } from "./Handoff";
import { KeyBar } from "./KeyBar";
import { NextSteps } from "./NextSteps";
import type { Glyph } from "./labels";
import { LinkPeek, type Peeked } from "./LinkPeek";
import { linksIn, rowText, type Found } from "./links";
import { Backoff, socketUrl } from "./socket";

/** How long a pane tries a terminal it has never attached before it says the terminal is not here. */
const NOT_HERE_AFTER = 10_000;
/** How long a connection must stay up before the wait between attaches drops back to its start. */
const STEADY_AFTER = 10_000;
/** How long a read of the workspace's changed files serves the links of every line the pointer crosses. */
const FILES_FRESH = 5_000;
/** A word with a file extension: only a line with one is worth reading the changed files for. */
const NAMES_FILE = /[\w-]\.[A-Za-z0-9]{1,10}\b/;

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
 *
 * A pane behind another tab (`active` false) stays attached while it is one of the few used last
 * (`ATTACHED_MAX`), so showing it again is instant and keeps its scroll position; it takes the keyboard each time it is shown, except on a touch
 * screen, where focus would raise the on-screen keyboard over what you came to read. There a
 * key bar (`name` and `glyph` say whose and in what state) carries the keys a phone lacks and
 * a box to write a message in.
 *
 * The screen is drawn with WebGL, so a program that redraws all the time, as Claude Code does,
 * leaves the page free to take keys; it is drawn with the DOM where the browser has no WebGL, and
 * from when the browser takes the WebGL context away.
 *
 * While `finding`, a find bar over the screen searches its whole scrollback.
 *
 * A terminal of this machine (`here`) can be handed files, as a desktop terminal can: dropped on
 * the pane, pasted (a screenshot on the clipboard), or picked with the key bar's clip on a phone.
 * Each is sent to the daemon, which keeps it beside the terminal, and its path is typed in
 * without Enter, so you add the words. Claude Code reads a pasted image path as the image.
 *
 * A session of this machine (`workspace` given, `here`) that is idle leads its key bar with its
 * workspace's next steps, each typed in as a prompt and sent.
 *
 * What a terminal prints can be opened, as in a desktop terminal: a web address, and, on this
 * machine, a file its workspace (`changesOf`) changed, by its path or its name. Resting the pointer
 * on one shows where it goes or the file's diff; ⌘- or Ctrl-click (a tap on a touch screen) opens
 * the address in a new tab or hands the file to `onFile`. A plain click still selects text. The
 * changed files are read only for a line the pointer is on that has a file-like word, and one read
 * serves every line for a few seconds.
 */
export const TerminalPane = memo(function TerminalPane({
  id,
  running,
  active = true,
  name,
  glyph,
  finding = 0,
  onFindClose = () => {},
  here = false,
  workspace,
  changesOf,
  onFile,
}: {
  id: string;
  running: boolean | undefined;
  active?: boolean;
  name?: string;
  glyph?: Glyph;
  /** Above 0, the find bar is open; it takes the keyboard again each time this changes. */
  finding?: number;
  onFindClose?: () => void;
  /** The terminal runs on the machine whose daemon serves the page, so it can be handed files. */
  here?: boolean;
  /** The workspace of the session the terminal runs; not given for a shell. */
  workspace?: string;
  /** The workspace the terminal (session or shell) belongs to, whose changed files its text can open. */
  changesOf?: string;
  /** Opens a changed file named on screen, by its path in the workspace's changes. */
  onFile?: (path: string) => void;
}) {
  const host = useRef<HTMLDivElement>(null);
  const shown = useRef<{ term: Terminal; fit: FitAddon } | null>(null);
  const termNow = useCallback(() => shown.current?.term, []);
  const [status, setStatus] = useState<string | null>(null);
  const runningNow = useRef(running);
  useEffect(() => {
    runningNow.current = running;
  }, [running]);
  const client = useQueryClient();
  const [peek, setPeek] = useState<Peeked | null>(null);
  /** What the links read when found and opened; the terminal outlives a change of these. */
  const linking = useRef({ client, changesOf, here, onFile });
  useEffect(() => {
    linking.current = { client, changesOf, here, onFile };
  }, [client, changesOf, here, onFile]);

  useEffect(() => {
    // As many lines as the holder keeps (xterm's default is 1,000), so find reaches all of them.
    // Decorations, which the find bar lights its hits with, are still a proposed API in xterm.
    // A link a program marks as one (OSC 8) opens the way a found one does, and only on the web.
    const linkHandler = {
      activate: (event: MouseEvent, text: string) => {
        if (wants(event) && /^https?:\/\//.test(text)) openUrl(text);
      },
      hover: (event: MouseEvent, text: string) => setPeek({ x: event.clientX, y: event.clientY, url: text }),
      leave: () => setPeek(null),
    };
    const term = new Terminal({ cursorBlink: true, fontSize: 13, fontFamily: "ui-monospace, Menlo, monospace", scrollback: 10_000, allowProposedApi: true, linkHandler });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host.current!);
    try {
      const webgl = new WebglAddon();
      webgl.onContextLoss(() => webgl.dispose());
      term.loadAddon(webgl);
    } catch {
      // No WebGL2 here: the DOM draws the screen.
    }
    fit.fit();
    shown.current = { term, fit };

    const links = term.registerLinkProvider({
      provideLinks(y, done) {
        const { text, cells } = rowText((row) => term.buffer.active.getLine(row), y - 1);
        const made = (found: Found[], files: FileChange[]): ILink[] =>
          found.map((f) => {
            const file = files.find((c) => c.path === f.target);
            const [from, to] = [cells[f.start], cells[f.end - 1]];
            return {
              range: { start: { x: from.x + 1, y: from.y + 1 }, end: { x: to.x + 1, y: to.y + 1 } },
              text: text.slice(f.start, f.end),
              activate: (event) => {
                if (!wants(event)) return;
                setPeek(null);
                if (f.kind === "url") openUrl(f.target);
                else linking.current.onFile?.(f.target);
              },
              hover: (event) => setPeek(file ? { x: event.clientX, y: event.clientY, file } : { x: event.clientX, y: event.clientY, url: f.target }),
              leave: () => setPeek(null),
            };
          });
        const { client, changesOf, here } = linking.current;
        if (!changesOf || !here || !NAMES_FILE.test(text)) return done(made(linksIn(text, []), []));
        client.fetchQuery({ ...changesQuery(changesOf), staleTime: FILES_FRESH }).then(
          (changes) => done(made(linksIn(text, changes.files.map((f) => f.path)), changes.files)),
          () => done(made(linksIn(text, []), [])),
        );
      },
    });

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
        if (opened && event.code === 1000) return setStatus("Ended");
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
      links.dispose();
      setPeek(null);
      socket?.close();
      term.dispose();
      shown.current = null;
      setStatus(null);
    };
  }, [id]);

  const [dragging, setDragging] = useState<number | null>(null);
  const [sending, setSending] = useState<string[] | null>(null);
  const [trouble, setTrouble] = useState<string | null>(null);
  const refusal = !here ? "Files go only to terminals on this machine" : running === false ? "This terminal is no longer running" : null;
  const hand = async (files: File[]) => {
    if (files.length === 0 || refusal) return;
    const big = files.find((f) => f.size > MAX_FILE);
    if (big) return setTrouble(`${big.name} is over ${MAX_FILE >> 20} MB; nothing was sent`);
    setTrouble(null);
    setSending(files.map((f) => f.name));
    const paths: string[] = [];
    try {
      for (const file of files) {
        try {
          paths.push(await sendFile(id, file));
        } catch (e) {
          return setTrouble(`Could not send ${file.name}: ${e instanceof Error ? e.message : String(e)}`);
        }
      }
    } finally {
      setSending(null);
    }
    const term = shown.current?.term;
    if (!term) return;
    term.paste(`${paths.join(" ")} `);
    if (!touch()) term.focus();
  };
  /** Types `text` in as one paste and presses Enter, as a message is sent. */
  const say = (text: string) => {
    shown.current?.term.paste(text);
    shown.current?.term.input("\r", true);
  };
  const withFiles = (types: readonly string[] | DOMStringList | undefined) => !!types && Array.from(types).includes("Files");

  // A pane kept open behind another tab takes the keyboard, and the size it missed, when shown.
  useEffect(() => {
    if (!active || !shown.current) return;
    shown.current.fit.fit();
    if (!touch()) shown.current.term.focus();
  }, [active, id]);

  return (
    <div
      data-testid="terminal-pane"
      className="flex h-full flex-col bg-black"
      onDragEnter={(e) => {
        if (!withFiles(e.dataTransfer?.types)) return;
        e.preventDefault();
        setDragging(Array.from(e.dataTransfer.items ?? []).filter((i) => i.kind === "file").length || 1);
      }}
      onDragOver={(e) => {
        if (!withFiles(e.dataTransfer?.types)) return;
        e.preventDefault();
        e.dataTransfer.dropEffect = refusal ? "none" : "copy";
        setDragging((n) => n ?? 1);
      }}
      onDragLeave={(e) => {
        if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setDragging(null);
      }}
      onDrop={(e) => {
        if (!withFiles(e.dataTransfer?.types)) return;
        // Never let the browser open the file in place of the app.
        e.preventDefault();
        setDragging(null);
        void hand(Array.from(e.dataTransfer.files));
      }}
      onPasteCapture={(e) => {
        const files = Array.from(e.clipboardData?.files ?? []);
        if (files.length === 0 || refusal) return;
        // Ahead of xterm, which would paste the clipboard's text, if any, instead.
        e.preventDefault();
        e.stopPropagation();
        void hand(files);
      }}
    >
      {/* The fit add-on sizes the terminal to its parent's height, padding included, so the padding sits outside it. */}
      <div className="relative min-h-0 flex-1 p-2">
        <div ref={host} className="h-full" />
        {finding > 0 && <FindBar key={id} term={termNow} name={name ?? "terminal"} ask={finding} onClose={onFindClose} />}
        {dragging !== null && <DropOverlay name={name ?? "terminal"} count={dragging} refusal={refusal} />}
        {sending && <SendingChip names={sending} />}
        {peek && <LinkPeek peek={peek} />}
      </div>
      {running !== false && (
        <KeyBar
          name={name ?? "terminal"}
          glyph={glyph ?? "ready"}
          modes={() => shown.current?.term.modes ?? { applicationCursorKeysMode: false }}
          onKey={(data) => shown.current?.term.input(data, true)}
          onMessage={say}
          onFiles={here ? (files) => void hand(files) : undefined}
          lead={workspace && here && glyph === "ready" ? <NextSteps workspace={workspace} name={name ?? "terminal"} send={say} look="keys" /> : undefined}
        />
      )}
      {trouble && (
        <div className="flex items-center gap-2 border-t border-neutral-800 px-3 py-1 text-xs text-amber-400">
          <span className="min-w-0 flex-1 truncate">{trouble}</span>
          <button type="button" aria-label="Dismiss" onClick={() => setTrouble(null)} className="text-neutral-500 hover:text-neutral-200">
            ×
          </button>
        </div>
      )}
      {status && <div className="border-t border-neutral-800 px-3 py-1 text-xs text-amber-400">{status}</div>}
    </div>
  );
});

/** Whether a click on a link opens it: with ⌘ or Ctrl held, so a plain click still selects; any tap on a touch screen. */
function wants(event: MouseEvent): boolean {
  return event.metaKey || event.ctrlKey || touch();
}

function openUrl(url: string) {
  window.open(url, "_blank", "noopener,noreferrer");
}

/** Whether the main pointer is a finger. */
function touch(): boolean {
  return typeof matchMedia === "function" && matchMedia("(pointer: coarse)").matches;
}
