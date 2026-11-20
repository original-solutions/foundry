import type { IDisposable, Terminal } from "@xterm/xterm";
import { useEffect, useRef, useState } from "react";
import { closest, hitsIn, MAX_HITS, marks, step, type BufferRow, type Hit } from "./find";
import { jumpKey } from "./JumpPalette";

/** The key that finds text in the open terminal. */
export const findKey = jumpKey === "⌘K" ? "⌘F" : "Ctrl F";

/** What the find bar needs of a terminal. */
export type Findable = {
  rows: number;
  buffer: {
    active: {
      length: number;
      viewportY: number;
      baseY: number;
      cursorY: number;
      getLine(row: number): { isWrapped: boolean; translateToString(trimRight?: boolean): string } | undefined;
    };
  };
} & Pick<Terminal, "registerMarker" | "registerDecoration" | "scrollToLine" | "focus" | "onWriteParsed" | "onScroll">;

/** The cell colours of a hit, and of the hit you are on (xterm takes only #RRGGBB). */
export const HIT = { backgroundColor: "#92400e", foregroundColor: "#fef3c7" };
export const CURRENT = { backgroundColor: "#7dd3fc", foregroundColor: "#000000" };

/** How long new output waits before the hits are found again, so a busy agent costs one search per beat. */
const AGAIN_AFTER = 250;

type Found = { hits: Hit[]; at: number; total: number };

function read(term: Findable, query: string): { hits: Hit[]; total: number } {
  const buffer = term.buffer.active;
  const rows: BufferRow[] = [];
  for (let i = 0; i < buffer.length; i++) {
    const line = buffer.getLine(i);
    rows.push({ text: line?.translateToString(false) ?? "", wrapped: line?.isWrapped ?? false });
  }
  return { hits: hitsIn(rows, query), total: buffer.length };
}

/**
 * Lights the hits on and around the screen (a screen above and below, so a scroll shows them at
 * once) and the one you are on brighter, dropping the last paint. Returns what to drop next time.
 */
function paint(term: Findable, found: Found, last: IDisposable[]): IDisposable[] {
  last.forEach((d) => d.dispose());
  const { viewportY, baseY, cursorY } = term.buffer.active;
  const painted: IDisposable[] = [];
  found.hits.forEach((hit, i) => {
    if (i !== found.at && (hit.row < viewportY - term.rows || hit.row > viewportY + 2 * term.rows)) return;
    const marker = term.registerMarker(hit.row - (baseY + cursorY));
    const decoration = term.registerDecoration({ marker, x: hit.col, width: hit.length, ...(i === found.at ? CURRENT : HIT) });
    painted.push(marker);
    if (decoration) painted.push(decoration);
  });
  return painted;
}

/** Scrolls the hit you are on to the middle of the screen, unless it is on screen already. */
function reveal(term: Findable, found: Found) {
  const hit = found.hits[found.at];
  const top = term.buffer.active.viewportY;
  if (hit && (hit.row < top || hit.row >= top + term.rows)) term.scrollToLine(Math.max(0, hit.row - Math.floor(term.rows / 2)));
}

/**
 * Find in a terminal's whole scrollback, as in an editor: every hit is counted, the one you are
 * on is lit and brought into view, Enter goes to the older one and Shift+Enter the newer. A
 * rail along the terminal's right edge marks where every hit sits, and a mark jumps to it. The
 * hits follow new output while the bar is open. It reads only what xterm already holds.
 *
 * `ask` changes each time the find key is pressed, and the box takes the keyboard again.
 */
export function FindBar({ term, name, ask, onClose }: { term: () => Findable | undefined; name: string; ask: number; onClose: () => void }) {
  const [query, setQuery] = useState("");
  const [found, setFound] = useState<Found>({ hits: [], at: 0, total: 0 });
  const box = useRef<HTMLInputElement>(null);
  const foundNow = useRef(found);
  const painted = useRef<IDisposable[]>([]);
  useEffect(() => {
    foundNow.current = found;
  }, [found]);

  useEffect(() => {
    box.current?.focus();
    box.current?.select();
  }, [ask]);

  // New output moves and adds hits: find them again, staying on the hit you were on.
  useEffect(() => {
    const t = term();
    if (!t || !query) return;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const written = t.onWriteParsed(() => {
      timer ??= setTimeout(() => {
        timer = undefined;
        const { hits, total } = read(t, query);
        const was = foundNow.current.hits[foundNow.current.at];
        const next = { hits, total, at: was ? closest(hits, was) : Math.max(0, hits.length - 1) };
        setFound(next);
        painted.current = paint(t, next, painted.current);
      }, AGAIN_AFTER);
    });
    // A scroll brings other hits on screen.
    const scrolled = t.onScroll(() => (painted.current = paint(t, foundNow.current, painted.current)));
    return () => {
      written.dispose();
      scrolled.dispose();
      clearTimeout(timer);
    };
  }, [term, query]);

  useEffect(() => () => painted.current.forEach((d) => d.dispose()), []);

  const go = (next: Found) => {
    setFound(next);
    const t = term();
    if (!t) return;
    reveal(t, next);
    painted.current = paint(t, next, painted.current);
  };
  const search = (text: string) => {
    setQuery(text);
    const t = term();
    const { hits, total } = t && text ? read(t, text) : { hits: [], total: 0 };
    go({ hits, total, at: Math.max(0, hits.length - 1) });
  };
  const move = (dir: 1 | -1) => found.hits.length > 0 && go({ ...found, at: step(found.at, found.hits.length, dir) });
  const close = () => {
    painted.current.forEach((d) => d.dispose());
    painted.current = [];
    const t = term();
    if (!(typeof matchMedia === "function" && matchMedia("(pointer: coarse)").matches)) t?.focus();
    onClose();
  };

  const n = found.hits.length;
  return (
    <>
      <div
        role="search"
        aria-label={`Find in ${name}`}
        className="absolute inset-x-2 top-2 z-10 flex items-center gap-1 rounded-lg border border-neutral-700 bg-neutral-900/95 p-1 pl-2.5 shadow-lg shadow-black/40 backdrop-blur sm:left-auto sm:w-96"
      >
        <svg viewBox="0 0 16 16" className="size-3.5 shrink-0 text-neutral-500" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
          <circle cx="7" cy="7" r="4.25" />
          <path d="M10.25 10.25L13.5 13.5" strokeLinecap="round" />
        </svg>
        <input
          ref={box}
          type="search"
          aria-label={`Find in ${name}`}
          placeholder={`Find in ${name}…`}
          value={query}
          autoComplete="off"
          autoCorrect="off"
          autoCapitalize="off"
          spellCheck={false}
          onChange={(e) => search(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              move(e.shiftKey ? 1 : -1);
            } else if (e.key === "Escape") {
              e.preventDefault();
              e.stopPropagation();
              close();
            }
          }}
          className="min-w-0 flex-1 bg-transparent py-1 text-sm text-neutral-100 placeholder:text-neutral-500 focus:outline-none [&::-webkit-search-cancel-button]:hidden"
        />
        <span role="status" className={`shrink-0 px-1 font-mono text-xs tabular-nums ${query && n === 0 ? "text-amber-400" : "text-neutral-400"}`}>
          {!query ? "" : n === 0 ? "No matches" : `${found.at + 1} of ${n}${n === MAX_HITS ? "+" : ""}`}
        </span>
        <BarButton label="Older match" title="Older match (Enter)" disabled={n === 0} onClick={() => move(-1)} path="M4 10l4-4 4 4" />
        <BarButton label="Newer match" title="Newer match (Shift+Enter)" disabled={n === 0} onClick={() => move(1)} path="M4 6l4 4 4-4" />
        <BarButton label="Close find" title="Close (Esc)" onClick={close} path="M4.5 4.5l7 7M11.5 4.5l-7 7" />
      </div>
      {n > 0 && (
        <div aria-hidden="true" className="absolute inset-y-2 right-0 z-10 w-2">
          {marks(found.hits, found.total, found.at).map((mark) => (
            <span
              key={mark.at}
              data-mark=""
              data-current={mark.current}
              style={{ top: `${mark.at}%` }}
              onClick={() => go({ ...found, at: mark.hit })}
              className="absolute inset-x-0 -mt-1 flex h-2 cursor-pointer items-center justify-center"
            >
              <span className={`block w-full rounded-full ${mark.current ? "h-1 bg-sky-300" : "h-0.5 bg-amber-400/80"}`} />
            </span>
          ))}
        </div>
      )}
    </>
  );
}

function BarButton({ label, title, path, disabled, onClick }: { label: string; title: string; path: string; disabled?: boolean; onClick: () => void }) {
  return (
    <button
      type="button"
      aria-label={label}
      title={title}
      disabled={disabled}
      onPointerDown={(e) => e.preventDefault()}
      onClick={onClick}
      className="shrink-0 rounded p-1.5 text-neutral-400 hover:bg-neutral-800 hover:text-neutral-100 disabled:opacity-40 disabled:hover:bg-transparent"
    >
      <svg viewBox="0 0 16 16" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
        <path d={path} strokeLinecap="round" strokeLinejoin="round" />
      </svg>
    </button>
  );
}
