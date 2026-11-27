import { useQueries } from "@tanstack/react-query";
import { useLayoutEffect, useRef, useState } from "react";
import { sessionTrail, type AnyRecord, type FactoryStatus, type Mark } from "./api";
import { AWAY_MS, keepLeft, leaving, moved, since, takeLeft, type Left, type Moved } from "./absence";
import type { MachineView } from "./fleet";
import { addsSteps, plural } from "./labels";
import { Named, Now } from "./Now";
import { pollEvery } from "./poll";
import { glyphText, StateGlyph } from "./StateGlyph";
import { STEPS_EVERY } from "./StepsPanel";
import { useRereadOnChange } from "./Timeline";
import { ago, tally } from "./trail";

/**
 * Coming back to the page after it was hidden for `AWAY_MS` or more (another tab, another app on a
 * phone, the tab closed and opened again) says what happened meanwhile, in a card heading the
 * page's notifications:
 * how long you were away, and a line for each session whose record moved and each that needs you,
 * most urgent first: its state, the turns it ended and the tools it ran since, and the question it
 * waits on or what Claude said at the end of its turn. A line opens its terminal and leaves the
 * card; Dismiss or Escape closes it. Nothing shows when no session moved.
 *
 * When the page was hidden is kept in this browser only, with each session's record version then;
 * which sessions moved is read from the versions, what they did from their trails since that time.
 */
export function Away({
  records,
  synced,
  machines,
  traced,
  onOpen,
  onShown,
  statuses,
}: {
  records: AnyRecord[];
  synced: boolean;
  machines: MachineView[];
  traced: Set<string>;
  onOpen: (terminal: string) => void;
  /** The sessions the card lists now, so news of them raises no card of its own. */
  onShown?: (sessions: string[]) => void;
  /** The believed factory statuses by project id: a live run's sessions need no one, as on the board. */
  statuses?: Record<string, FactoryStatus>;
}) {
  const left = useLeft(records, synced);
  const [gone, setGone] = useState<{ of?: number; ids: string[] }>({ ids: [] });
  const dismissed = gone.of === left.since?.at ? gone.ids : [];
  const list = left.since && synced ? moved(left.since, records, machines, statuses).filter((m) => !dismissed.includes(m.row.session)) : [];
  const trails = useQueries({
    queries: list.map((m) => ({
      enabled: traced.has(m.row.session),
      // The steps column's key, so they share what they read.
      queryKey: ["trail", m.row.session],
      queryFn: () => sessionTrail(m.row.session),
      retry: false,
      refetchInterval: pollEvery(STEPS_EVERY, addsSteps.includes(m.row.glyph)),
    })),
  });
  useRereadOnChange(list.map((m) => `${m.row.session}=${m.row.label}`).join("\n"));
  const shown = left.since ? list.map((m) => m.row.session).join(" ") : "";
  const tell = useRef(onShown);
  useLayoutEffect(() => {
    tell.current = onShown;
  });
  useLayoutEffect(() => {
    tell.current?.(shown ? shown.split(" ") : []);
  }, [shown]);
  if (!left.since || list.length === 0) return null;
  const from = left.since.at;

  const did = trails.map((t) => since(t.data ?? [], from));
  const turns = did.reduce((n, d) => n + d.turns, 0);
  const tools = did.reduce((n, d) => n + d.steps.filter((s) => s.kind === "tool").length, 0);
  const need = list.filter((m) => m.needs).length;
  const sum = [plural(list.length, "session"), turns > 0 && plural(turns, "turn"), tools > 0 && plural(tools, "tool call"), need > 0 && `${need} need${need === 1 ? "s" : ""} you`].filter(Boolean).join(" · ");

  const open = (m: Moved) => {
    onOpen(m.row.terminal);
    if (list.length === 1) left.close();
    else setGone({ of: from, ids: [...dismissed, m.row.session] });
  };

  return (
    <aside
      aria-label="While you were away"
      onKeyDown={(event) => event.key === "Escape" && left.close()}
      className="pointer-events-auto flex max-h-[min(32rem,55dvh)] flex-col overflow-hidden rounded-xl border border-neutral-700/70 bg-neutral-900 text-sm text-neutral-100 shadow-2xl shadow-black/50 motion-safe:animate-sheet-in"
    >
      <header className="flex items-start gap-3 border-b border-neutral-800 px-4 pt-3 pb-2.5">
        <div className="min-w-0 flex-1">
          <h2 className="flex items-baseline gap-2 font-semibold">
            While you were away
            <span className="rounded bg-sky-400/15 px-1.5 font-mono text-[11px] font-medium text-sky-300">{ago(from, left.since.back)}</span>
          </h2>
          <p className="mt-0.5 text-xs text-neutral-400">{sum}</p>
        </div>
        <button type="button" aria-label="Dismiss" title="Dismiss (Esc)" onClick={left.close} className="-mr-1 rounded p-1 text-neutral-500 hover:bg-neutral-800 hover:text-neutral-200">
          <svg viewBox="0 0 16 16" className="size-4" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
            <path d="M4 4l8 8M12 4l-8 8" strokeLinecap="round" />
          </svg>
        </button>
      </header>
      <ul className="flex flex-col gap-0.5 overflow-y-auto p-1.5">
        {list.map((m, i) => (
          <li key={m.row.session}>
            <Line m={m} turns={did[i].turns} steps={did[i].steps} onOpen={() => open(m)} />
          </li>
        ))}
      </ul>
    </aside>
  );
}

function Line({ m, turns, steps, onOpen }: { m: Moved; turns: number; steps: Mark[]; onOpen: () => void }) {
  const { row } = m;
  const what = [turns > 0 && plural(turns, "turn"), steps.length > 0 && tally(steps)].filter(Boolean).join(" · ");
  return (
    <button
      type="button"
      aria-label={`Open ${row.name} in ${m.workspace}: ${row.label}`}
      onClick={onOpen}
      className={`flex w-full flex-col gap-0.5 rounded-lg px-2.5 py-2 text-left hover:bg-neutral-800/70 focus-visible:outline-2 focus-visible:outline-sky-400 ${m.needs ? "bg-amber-400/5" : ""}`}
    >
      <span className="flex items-center gap-2">
        <StateGlyph glyph={row.glyph} />
        <span className="flex min-w-0 flex-1">
          <Named name={row.name} title={row.title} />
        </span>
        <span className={`shrink-0 text-xs ${glyphText[row.glyph]}`}>{row.label}</span>
      </span>
      <span className="flex gap-1.5 pl-5 text-[11px] text-neutral-500">
        <span className="truncate">{m.workspace}</span>
        {m.fresh && <span className="rounded bg-emerald-400/10 px-1 font-medium text-emerald-300">New</span>}
        {what && <span className="truncate text-neutral-400">· {what}</span>}
      </span>
      {row.now ? (
        <Now step={row.now} className="pl-5" />
      ) : (
        row.said && <span className="line-clamp-2 border-l border-neutral-700 ml-5 pl-2 text-xs whitespace-pre-line text-neutral-300">{row.said}</span>
      )}
    </button>
  );
}

/**
 * When the page was last hidden, once it is shown again after `AWAY_MS` or more: kept on hiding
 * (only once the fleet is known, so every session has a version), and read once on opening the page
 * in case the tab was closed while hidden. A second absence while the card is up keeps the first's
 * start, so the card covers both.
 */
function useLeft(records: AnyRecord[], synced: boolean): { since?: Left & { back: number }; close: () => void } {
  const [left, setLeft] = useState<Left & { back: number }>();
  const latest = useRef({ records, synced });
  useLayoutEffect(() => {
    latest.current = { records, synced };
  });
  useLayoutEffect(() => {
    let held: Left | undefined;
    const cameBack = (from: Left | undefined) => {
      const back = Date.now();
      if (from && back - from.at >= AWAY_MS) setLeft((open) => (open ? { ...open, back } : { ...from, back }));
    };
    const hide = () => {
      if (held || !latest.current.synced) return;
      held = leaving(latest.current.records, Date.now());
      keepLeft(held);
    };
    const change = () => {
      if (document.visibilityState === "hidden") return hide();
      const from = held ?? takeLeft();
      takeLeft();
      held = undefined;
      cameBack(from);
    };
    if (document.visibilityState === "visible") cameBack(takeLeft());
    document.addEventListener("visibilitychange", change);
    window.addEventListener("pagehide", hide);
    return () => {
      document.removeEventListener("visibilitychange", change);
      window.removeEventListener("pagehide", hide);
    };
  }, []);
  return { since: left, close: () => setLeft(undefined) };
}
