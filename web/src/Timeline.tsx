import { useQueries, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { sessionTrail } from "./api";
import { rows, type MachineView } from "./fleet";
import { addsSteps, busy, plural, type Glyph } from "./labels";
import { Named } from "./Now";
import { pollEvery } from "./poll";
import { glyphText, StateGlyph } from "./StateGlyph";
import { STEPS_EVERY, useNow } from "./StepsPanel";
import { bars, heading, placed, RANGES, sum, ticks, type Bar } from "./spans";
import { ago, kindOf, tally, took } from "./trail";
import { readStored, store } from "./storage";

/** A turn still going, coloured as its session's state; a finished one is quiet. */
const openLook: Partial<Record<Glyph, string>> = {
  working: "bg-sky-400 border-sky-300",
  waiting: "bg-amber-400 border-amber-300",
  stuck: "bg-orange-400 border-orange-300",
};
const doneLook = "bg-sky-400/25 border-sky-400/50 hover:bg-sky-400/40";

const windowName: Record<string, string> = { "15m": "the last 15 minutes", "1h": "the last hour", "4h": "the last 4 hours", "12h": "the last 12 hours" };

/** The window the timeline was last set to, kept for this browser. */
const RANGE_KEY = "foundry.timeline";
function loadRange(): (typeof RANGES)[number] {
  const kept = readStored(RANGE_KEY);
  return RANGES.find((r) => r.key === kept) ?? RANGES[1];
}

interface Lane {
  session: string;
  terminal: string;
  name: string;
  title?: string;
  workspace: string;
  glyph: Glyph;
  label: string;
  bars: Bar[];
}

/**
 * Every session of the fleet on one clock: a lane per session that took a turn in the window,
 * each turn a bar from its prompt to its end, with a tick for each tool it ran and an amber dot
 * for each question it asked (a grey one for a reminder between turns). A turn still going runs to the "now" edge in its session's colour.
 * Pointing at a bar says what the turn was and did; clicking it opens the terminal with its
 * steps. Read from the trails of the sessions this machine's daemon keeps them for (`traced`, the
 * ones it has briefs of), so a peer's session is never asked. A trail is read as the steps column
 * reads it, under the same entry: again whenever its session changes state, and every few seconds
 * while it starts or works.
 */
export function Timeline({ machines, traced, onSteps }: { machines: MachineView[]; traced: Set<string>; onSteps: (terminal: string) => void }) {
  const [range, setRange] = useState(loadRange);
  const [pointed, setPointed] = useState<string>();
  const sessions = machines.flatMap((m) =>
    m.workspaces.flatMap((w) => rows(w).flatMap((r) => (r.session ? [{ ...r, session: r.session, workspace: w.name }] : []))),
  );
  const live = sessions.some((s) => busy.includes(s.glyph));
  const now = useNow(live ? 1000 : 15_000);
  const trails = useQueries({
    queries: sessions.map((s) => ({
      enabled: traced.has(s.session),
      // The steps column's key, so the two share what they read.
      queryKey: ["trail", s.session],
      queryFn: () => sessionTrail(s.session),
      retry: false,
      refetchInterval: pollEvery(STEPS_EVERY, addsSteps.includes(s.glyph)),
    })),
  });
  useRereadOnChange(sessions.map((s) => `${s.session}=${s.label}`).join("\n"));
  const from = now - range.ms;
  const lanes: Lane[] = sessions
    .map((s, i) => ({ ...s, bars: bars(trails[i].data ?? [], busy.includes(s.glyph), now).filter((b) => placed(b, from, now)) }))
    .filter((l) => l.bars.some((b) => !b.nudge))
    .sort((a, b) => Number(b.bars.at(-1)!.open) - Number(a.bars.at(-1)!.open) || b.bars.at(-1)!.end - a.bars.at(-1)!.end);
  const quiet = sessions.length - lanes.length;
  const total = sum(
    lanes.map((l) => l.bars),
    from,
    now,
  );
  const where = windowName[range.key];
  const pick = (next: (typeof RANGES)[number]) => {
    setRange(next);
    // When not kept, the window still changes.
    store(RANGE_KEY, next.key);
  };

  return (
    <section aria-label="Timeline" className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center gap-x-4 gap-y-2 px-1">
        <p className="min-w-0 flex-1 text-sm text-neutral-300">
          <span className="text-neutral-500">In {where}: </span>
          {lanes.length > 0 && (
            <span>
              {[plural(total.turns, "turn"), `${took(total.busy)} of agent work`, plural(total.tools, "tool call"), total.asks > 0 ? plural(total.asks, "question") : undefined]
                .filter(Boolean)
                .join(" · ")}
            </span>
          )}
        </p>
        <div role="radiogroup" aria-label="Window" className="flex rounded-lg border border-neutral-800 bg-neutral-900 p-0.5 text-xs">
          {RANGES.map((r) => (
            <button
              key={r.key}
              type="button"
              role="radio"
              aria-checked={r.key === range.key}
              onClick={() => pick(r)}
              className={`rounded-md px-2.5 py-1 font-medium transition-colors ${r.key === range.key ? "bg-neutral-700 text-neutral-50" : "text-neutral-400 hover:text-neutral-200"}`}
            >
              {r.key}
            </button>
          ))}
        </div>
      </div>

      {lanes.length === 0 ? (
        <div className="rounded-xl border border-dashed border-neutral-800 px-6 py-12 text-center text-sm text-neutral-500">
          {`No agent took a turn in ${where}. Each prompt shows here as a bar while it runs; turns are kept from when this machine's daemon started.`}
        </div>
      ) : (
        <div className="relative rounded-xl border border-neutral-800 bg-neutral-900/40 pb-2">
          {/* The clock: ticks back from now, and a gridline for each down the lanes. */}
          <div className="pointer-events-none absolute inset-0 grid grid-cols-[9rem_1fr] lg:grid-cols-[16rem_1fr] pr-4">
            <span />
            <span className="relative">
              {ticks(range.ms).map((t) => (
                <span key={t.label} className="absolute inset-y-0 w-px bg-neutral-800/70" style={{ left: `${t.left}%` }} />
              ))}
              <span className="absolute inset-y-0 right-0 w-px bg-sky-400/60" />
            </span>
          </div>
          <div className="relative grid h-8 grid-cols-[9rem_1fr] lg:grid-cols-[16rem_1fr] items-end border-b border-neutral-800 pr-4 pb-1.5 text-[11px] text-neutral-500">
            <span className="px-4">Session</span>
            <span className="relative h-4">
              {ticks(range.ms).map((t) => (
                <span key={t.label} className="absolute -translate-x-1/2 font-mono" style={{ left: `${t.left}%` }}>
                  {t.label}
                </span>
              ))}
              <span className="absolute right-0 translate-x-1/2 rounded bg-sky-400/15 px-1 font-medium text-sky-300">now</span>
            </span>
          </div>
          <div className="relative flex flex-col">
            {lanes.map((lane) => (
              <LaneRow key={lane.session} lane={lane} from={from} to={now} now={now} pointed={pointed} onPoint={setPointed} onSteps={onSteps} />
            ))}
          </div>
        </div>
      )}
      {quiet > 0 && (
        <p className="px-1 text-xs text-neutral-500">
          {`${plural(quiet, "more session")} took no turns in ${where}`}
        </p>
      )}
    </section>
  );
}

/**
 * Reads a session's trail again when its state changes (a hook arrived), given as `id=label`
 * lines; a session seen for the first time is read by its query already.
 */
export function useRereadOnChange(states: string) {
  const client = useQueryClient();
  const last = useRef(new Map<string, string>());
  useEffect(() => {
    const next = new Map(states ? states.split("\n").map((line) => line.split("=", 2) as [string, string]) : []);
    for (const [id, label] of next) {
      const before = last.current.get(id);
      if (before !== undefined && before !== label) void client.invalidateQueries({ queryKey: ["trail", id], exact: true });
    }
    last.current = next;
  }, [states, client]);
}

function LaneRow({
  lane,
  from,
  to,
  now,
  pointed,
  onPoint,
  onSteps,
}: {
  lane: Lane;
  from: number;
  to: number;
  now: number;
  pointed?: string;
  onPoint: (key?: string) => void;
  onSteps: (terminal: string) => void;
}) {
  const named = `${lane.name}${lane.title ? ` ${lane.title}` : ""} · ${lane.workspace}`;
  return (
    <div role="group" aria-label={named} className="group/lane grid grid-cols-[9rem_1fr] lg:grid-cols-[16rem_1fr] items-center pr-4 hover:bg-neutral-800/30">
      <button
        type="button"
        onClick={() => onSteps(lane.terminal)}
        title={`${named}: ${lane.label}`}
        className="flex min-w-0 items-center gap-2 py-1.5 pr-3 pl-4 text-left text-sm text-neutral-200"
      >
        <StateGlyph glyph={lane.glyph} />
        <span className="min-w-0 flex-1 leading-tight">
          <span className="flex">
            <Named name={lane.name} title={lane.title} />
          </span>
          <span className="block truncate text-[11px] text-neutral-500">
            {lane.workspace} · <span className={glyphText[lane.glyph]}>{lane.label}</span>
          </span>
        </span>
      </button>
      <div className="relative h-9">
        <span aria-hidden="true" className="absolute inset-x-0 top-1/2 h-px bg-neutral-800" />
        {lane.bars.map((bar) => {
          if (bar.nudge) return null;
          const at = placed(bar, from, to)!;
          const key = `${lane.session}:${bar.start}`;
          const span = took(bar.end - bar.start);
          const label = [heading(bar), tally(bar.steps), bar.open ? `running for ${span}` : `took ${span}, ${ago(bar.start, now)} ago`].join(", ");
          const look = bar.open ? (openLook[lane.glyph] ?? "bg-neutral-400 border-neutral-300") : doneLook;
          return (
            <button
              key={key}
              type="button"
              aria-label={label}
              onClick={() => onSteps(lane.terminal)}
              onPointerEnter={() => onPoint(key)}
              onPointerLeave={() => onPoint(undefined)}
              onFocus={() => onPoint(key)}
              onBlur={() => onPoint(undefined)}
              className={`absolute top-2 bottom-2 rounded-[4px] border transition-colors focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-sky-400 ${look}`}
              style={{ left: `${at.left}%`, width: `max(${at.width}%, 5px)` }}
            />
          );
        })}
        {/* A tick for each tool call, an amber dot for each question, where they fell in time. */}
        {lane.bars.flatMap((bar) =>
          bar.steps.map((step, i) => {
            if (step.at < from || step.at > to) return null;
            const left = `${((step.at - from) / (to - from)) * 100}%`;
            return step.kind === "ask" ? (
              <span
                key={`${bar.start}-${i}`}
                aria-hidden="true"
                className={`pointer-events-none absolute top-0.5 size-1.5 -translate-x-1/2 rounded-full ${bar.nudge ? "bg-neutral-600" : "bg-amber-400 shadow-[0_0_4px_var(--color-amber-400)]"}`}
                style={{ left }}
              />
            ) : (
              <span key={`${bar.start}-${i}`} aria-hidden="true" className={`pointer-events-none absolute top-3 bottom-3 w-px ${kindOf(step) === "edit" ? "bg-amber-200/90" : "bg-sky-100/45"}`} style={{ left }} />
            );
          }),
        )}
        {lane.bars.map((bar) => {
          const key = `${lane.session}:${bar.start}`;
          if (bar.nudge || pointed !== key) return null;
          return <Peek key={key} bar={bar} now={now} left={placed(bar, from, to)!.left} />;
        })}
      </div>
    </div>
  );
}

/** What a pointed-at turn was: its prompt, what it did, how long it took and when. */
function Peek({ bar, now, left }: { bar: Bar; now: number; left: number }) {
  const span = took(bar.end - bar.start);
  // Hangs from the bar's start, or ends there when the bar sits in the right half.
  const place = left > 55 ? { right: `${100 - left}%` } : { left: `${left}%` };
  return (
    <span
      role="tooltip"
      style={place}
      className="pointer-events-none absolute bottom-full z-30 -mb-1 flex w-72 flex-col gap-1 rounded-lg border border-neutral-700 bg-neutral-900 px-3 py-2 text-left text-xs shadow-xl shadow-black/40"
    >
      <span className={`line-clamp-3 text-sm ${bar.prompt?.startsWith("<") ? "text-neutral-300 italic" : "text-neutral-100"}`}>{heading(bar)}</span>
      <span className="text-neutral-400">{tally(bar.steps)}</span>
      <span className="flex justify-between font-mono text-[11px] text-neutral-500">
        <span>{bar.open ? `running ${span}` : `took ${span}`}</span>
        <span>{ago(bar.start, now)} ago</span>
      </span>
      <span className="text-[11px] text-neutral-600">Click to open its steps</span>
    </span>
  );
}
