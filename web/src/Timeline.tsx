import { useQueries, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { sessionTrail } from "./api";
import { rows, type MachineView } from "./fleet";
import { addsSteps, busy, plural, type Glyph } from "./labels";
import { Named } from "./Now";
import { pollEvery } from "./poll";
import { glyphText, StateGlyph } from "./StateGlyph";
import { STEPS_EVERY, useNow } from "./StepsPanel";
import { bars, heading, layers, placed, RANGES, sum, ticks, waits, type Bar, type Wait } from "./spans";
import { ago, kindOf, tally, took } from "./trail";
import { readStored, store } from "./storage";

/**
 * A turn still going, coloured as its session's state; a finished one is quiet. A turn that waits
 * on you is drawn as the work it did, with the wait laid over it from the question on.
 */
const openLook: Partial<Record<Glyph, string>> = {
  working: "bg-sky-400 border-sky-300",
  waiting: "bg-sky-400 border-sky-300",
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
  /** The label uncut, when it cuts its factory run's reason short. */
  fullLabel?: string;
  bars: Bar[];
  /** When its turns in the window stood waiting on you. */
  waits: Wait[];
}

/** Amber stripes on the page's own colour: time a turn stood waiting on you, not time the agent worked. */
const stripes = {
  backgroundColor: "var(--color-neutral-950)",
  backgroundImage: "repeating-linear-gradient(135deg, var(--color-amber-400) 0 3px, color-mix(in oklab, var(--color-amber-400) 40%, transparent) 3px 6px)",
};

/**
 * Every session of the fleet on one clock: a lane per session that took a turn in the window,
 * each turn a bar from its prompt to its end, with a tick for each tool it ran and an amber dot
 * for each question it asked (a grey one for a reminder between turns). A turn still going runs to the "now" edge in its session's colour.
 * Pointing at a bar says what the turn was and did; clicking it opens the terminal with its
 * steps.
 *
 * A question splits its turn: from it to the step after it (or to now, while the session waits on
 * you) the turn stood waiting on you, drawn in amber stripes over the bar and counted apart from
 * agent work. Above the lanes, a "Waiting on you" row lays every session's waits over each other,
 * taller where more waited at once, so a glance says when the fleet stood waiting on you; while
 * any waits now, it opens the one that has waited longest.
 *
 * Read from the trails of the sessions this machine's daemon keeps them for (`traced`, the ones
 * it has briefs of), so a peer's session is never asked. A trail is read as the steps column
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
    .map((s, i) => {
      const shown = bars(trails[i].data ?? [], busy.includes(s.glyph), now).filter((b) => placed(b, from, now));
      return { ...s, bars: shown, waits: shown.flatMap((b) => waits(b, s.glyph === "waiting", now)) };
    })
    .filter((l) => l.bars.some((b) => !b.nudge))
    .sort((a, b) => Number(b.bars.at(-1)!.open) - Number(a.bars.at(-1)!.open) || b.bars.at(-1)!.end - a.bars.at(-1)!.end);
  const quiet = sessions.length - lanes.length;
  const total = sum(
    lanes.map((l) => l.bars),
    from,
    now,
    lanes.map((l) => l.waits),
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
              {[
                plural(total.turns, "turn"),
                `${took(total.busy)} of agent work`,
                total.waited > 0 ? `${took(total.waited)} waiting on you` : undefined,
                plural(total.tools, "tool call"),
                total.asks > 0 ? plural(total.asks, "question") : undefined,
              ]
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
            {total.waited > 0 && <YouRow lanes={lanes} from={from} to={now} waited={total.waited} where={where} onSteps={onSteps} />}
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
  const waitingNow = lane.waits.find((w) => w.open);
  return (
    <div role="group" aria-label={named} className="group/lane grid grid-cols-[9rem_1fr] lg:grid-cols-[16rem_1fr] items-center pr-4 hover:bg-neutral-800/30">
      <button
        type="button"
        onClick={() => onSteps(lane.terminal)}
        title={`${named}: ${lane.fullLabel ?? lane.label}`}
        className="flex min-w-0 items-center gap-2 py-1.5 pr-3 pl-4 text-left text-sm text-neutral-200"
      >
        <StateGlyph glyph={lane.glyph} />
        <span className="min-w-0 flex-1 leading-tight">
          <span className="flex">
            <Named name={lane.name} title={lane.title} />
          </span>
          <span className="block truncate text-[11px] text-neutral-500">
            {lane.workspace} ·{" "}
            <span title={lane.fullLabel} className={glyphText[lane.glyph]}>
              {lane.label}
            </span>
            {waitingNow && <span className={glyphText[lane.glyph]}>{` for ${took(now - waitingNow.start)}`}</span>}
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
          const label = [heading(bar), tally(bar.steps), bar.open ? `running for ${span}` : `took ${span}, ${ago(bar.start, now)} ago`, held(bar, lane, now)]
            .filter(Boolean)
            .join(", ");
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
        {/* The waits, over their bars: amber stripes from each question to its answer, or to now. */}
        {lane.waits.map((w) => {
          const at = placed(w, from, to);
          if (!at) return null;
          return (
            <span
              key={`wait-${w.start}`}
              aria-hidden="true"
              className={`pointer-events-none absolute top-2 bottom-2 rounded-[3px] ${w.open ? "shadow-[0_0_6px_color-mix(in_oklab,var(--color-amber-400)_50%,transparent)]" : "opacity-60"}`}
              style={{ ...stripes, left: `${at.left}%`, width: `max(${at.width}%, 3px)` }}
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
          return <Peek key={key} bar={bar} held={held(bar, lane, now)} now={now} left={placed(bar, from, to)!.left} />;
        })}
      </div>
    </div>
  );
}

/** What a pointed-at turn was: its prompt, what it did, how long it took and when. */
function Peek({ bar, held, now, left }: { bar: Bar; held?: string; now: number; left: number }) {
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
      {held && <span className="text-amber-300">{held.charAt(0).toUpperCase() + held.slice(1)}</span>}
      <span className="flex justify-between font-mono text-[11px] text-neutral-500">
        <span>{bar.open ? `running ${span}` : `took ${span}`}</span>
        <span>{ago(bar.start, now)} ago</span>
      </span>
      <span className="text-[11px] text-neutral-600">Click to open its steps</span>
    </span>
  );
}

/** How long a turn stood waiting on you, in words: still waiting, or waited in all; nothing when it never did. */
function held(bar: Bar, lane: Lane, now: number): string | undefined {
  const mine = lane.waits.filter((w) => w.start >= bar.start && w.end <= bar.end);
  const open = mine.find((w) => w.open);
  if (open) return `waiting on you for ${took(now - open.start)}`;
  const all = mine.reduce((n, w) => n + w.end - w.start, 0);
  return all > 0 ? `waited on you ${took(all)}` : undefined;
}

/**
 * The row over the lanes that adds up the waits: every session's waits laid over each other,
 * taller where more stood waiting at once, full height at the most in the window. While any waits now, its name opens the one that has
 * waited longest.
 */
function YouRow({ lanes, from, to, waited, where, onSteps }: { lanes: Lane[]; from: number; to: number; waited: number; where: string; onSteps: (terminal: string) => void }) {
  const open = lanes.flatMap((lane) => lane.waits.filter((w) => w.open).map((w) => ({ lane, start: w.start })));
  const longest = open.reduce<(typeof open)[number] | undefined>((a, b) => (a && a.start <= b.start ? a : b), undefined);
  const spans = layers(
    lanes.map((l) => l.waits),
    from,
    to,
  );
  const most = Math.max(1, ...spans.map((s) => s.depth));
  const words = (
    <>
      {/* A phone's narrow column keeps the words whole instead. */}
      <span className="hidden sm:contents">
        <StateGlyph glyph="waiting" />
      </span>
      <span className="min-w-0 flex-1 leading-tight">
        <span className="block truncate font-medium text-amber-300">Waiting on you</span>
        <span className="block truncate text-[11px] text-neutral-500">
          {took(waited)}
          <span className="hidden sm:inline"> in {where}</span>
          {open.length > 0 && <span className="text-amber-300">{` · ${open.length} now`}</span>}
        </span>
      </span>
    </>
  );
  return (
    <div role="group" aria-label="Waiting on you" className="grid grid-cols-[9rem_1fr] items-center border-b border-neutral-800 bg-amber-400/[0.04] pr-4 lg:grid-cols-[16rem_1fr]">
      {longest ? (
        <button
          type="button"
          onClick={() => onSteps(longest.lane.terminal)}
          aria-label={`Waiting on you: open ${longest.lane.name} in ${longest.lane.workspace}, waiting longest, ${took(to - longest.start)}`}
          title={`Open ${longest.lane.name} in ${longest.lane.workspace}, waiting longest`}
          className="flex min-w-0 items-center gap-2 py-1.5 pr-3 pl-4 text-left text-sm hover:bg-amber-400/10 focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-amber-400"
        >
          {words}
        </button>
      ) : (
        <div className="flex min-w-0 items-center gap-2 py-1.5 pr-3 pl-4 text-sm">{words}</div>
      )}
      <div aria-hidden="true" className="relative h-9">
        <span className="absolute inset-x-0 top-1/2 h-px bg-amber-400/15" />
        {spans.map((span) => {
          const at = placed(span, from, to)!;
          return (
            <span
              key={span.start}
              title={`${plural(span.depth, "session")} waiting on you`}
              className="absolute bottom-2 rounded-[3px]"
              style={{ ...stripes, left: `${at.left}%`, width: `max(${at.width}%, 3px)`, height: `${4 + (16 * span.depth) / most}px` }}
            />
          );
        })}
      </div>
    </div>
  );
}
