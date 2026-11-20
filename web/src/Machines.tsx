import { Link, useNavigate } from "@tanstack/react-router";
import { useEffect, useId, useState } from "react";
import { JumpBar, saveView, ViewSwitch } from "./Board";
import { byUrgency, rows, type MachineView } from "./fleet";
import { plural, urgency, type Glyph } from "./labels";
import { heardAgo, scaled, span, spanWords, type History, type Point } from "./samples";
import { glyphText, StateGlyph } from "./StateGlyph";

const statusText = { online: "text-emerald-400", offline: "text-red-400", unknown: "text-neutral-400" };
const statusDot = { online: "bg-emerald-400", offline: "bg-red-400", unknown: "bg-neutral-500" };

/** A count of terminals in a state, in words. */
const stateWord: Record<Glyph, string> = {
  waiting: "needs you",
  stuck: "stuck",
  failed: "failed",
  working: "working",
  starting: "starting",
  ready: "ready",
  done: "ended",
  away: "unreachable",
};
const stateBar: Record<Glyph, string> = {
  waiting: "bg-amber-400",
  stuck: "bg-orange-400",
  failed: "bg-red-400",
  working: "bg-sky-400",
  starting: "bg-neutral-400",
  ready: "bg-emerald-400/60",
  done: "bg-neutral-600",
  away: "bg-neutral-700",
};

/** Load per core: green with room, amber near full, red past one per core. */
const loadTone = (load: number) => (load >= 1 ? "text-red-400" : load >= 0.7 ? "text-amber-400" : "text-emerald-400");
const gb = (bytes: number) => `${(bytes / 1e9).toFixed(1)} GB`;

/** This browser's clock, moved on every second, for "heard 4 s ago" and the lines' sliding window. */
function useNow() {
  const [now, setNow] = useState(Date.now);
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);
  return now;
}

interface Props {
  machines: MachineView[];
  /** Each machine's samples since this page opened. */
  history: History;
  /** The machine whose daemon serves this page. */
  here?: string;
  connected: boolean;
  onJump?: () => void;
}

/**
 * Every machine in the fleet as a card: online or offline and when this page last heard from it,
 * its load per core drawn over the last ten minutes (or since the page opened), its free memory and disk, its terminals
 * counted by state and its workspaces, each opening its most urgent terminal. An offline machine
 * keeps its card, greyed, with its last known numbers; nothing of it is hidden.
 */
export function Machines({ machines, history, here, connected, onJump }: Props) {
  const now = useNow();
  const navigate = useNavigate();
  const online = machines.filter((m) => m.status === "online").length;
  const offline = machines.filter((m) => m.status === "offline").length;
  const counts = [`${plural(machines.length, "machine")}`, `${online} online`, ...(offline ? [`${offline} offline`] : [])].join(" · ");
  return (
    <div className="absolute inset-0 overflow-y-auto">
      <section aria-label="Machines" className="mx-auto flex max-w-7xl flex-col gap-6 px-4 pt-4 pb-16 md:px-6 md:pt-6">
        <div className="flex flex-col gap-2">
          <Link to="/" className="flex items-center gap-1 self-start rounded px-1 text-sm text-neutral-400 hover:text-neutral-100 md:hidden">
            <svg viewBox="0 0 16 16" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" aria-hidden="true">
              <path d="M10 3.5L5.5 8l4.5 4.5" />
            </svg>
            Workspaces
          </Link>
          {onJump && <JumpBar onJump={onJump} />}
          <div className="flex items-center gap-3 px-1">
            <p className="min-w-0 flex-1 text-xs text-neutral-500">{counts}</p>
            <ViewSwitch
              view="machines"
              onPick={(view) => {
                if (view === "machines") return;
                saveView(view);
                void navigate({ to: "/" });
              }}
            />
          </div>
          {!connected && (
            <p role="status" className="rounded-lg border border-amber-400/30 bg-amber-400/10 px-3 py-2 text-xs text-amber-300">
              Disconnected from the daemon: these are the last numbers this page saw, until it reconnects.
            </p>
          )}
        </div>
        <div className="grid grid-cols-[repeat(auto-fit,minmax(min(100%,24rem),1fr))] gap-4">
          {machines.map((machine) => (
            <MachineCard key={machine.id} machine={machine} points={history[machine.id]?.points ?? []} heard={history[machine.id]?.heard} here={machine.id === here} now={now} />
          ))}
        </div>
      </section>
    </div>
  );
}

function MachineCard({ machine, points, heard, here, now }: { machine: MachineView; points: Point[]; heard?: number; here: boolean; now: number }) {
  const id = useId();
  const { sample, status } = machine;
  const away = status === "offline";
  const all = machine.workspaces.flatMap(rows);
  const ago = away ? undefined : heardAgo(heard, now);
  const loads = points.map((p) => p.load);
  const memories = points.map((p) => p.memory);
  // An offline machine's lines stop where it was last heard.
  const end = away ? (points.at(-1)?.at ?? now) : now;
  const covers = span(points, end);
  return (
    <article
      aria-labelledby={id}
      className={`relative flex flex-col overflow-hidden rounded-xl border bg-neutral-900/60 ${away ? "border-red-400/30" : here ? "border-neutral-700" : "border-neutral-800"}`}
    >
      <header className="flex flex-wrap items-center gap-x-2 gap-y-1 px-4 pt-3.5">
        <span aria-hidden="true" className={`size-2 shrink-0 rounded-full ${statusDot[status]}`} />
        <h3 id={id} className="min-w-0 truncate text-[15px] font-semibold text-neutral-100" title={machine.id}>
          {machine.name}
        </h3>
        {here && <span className="rounded-full bg-neutral-800 px-2 py-px text-[11px] font-medium text-neutral-300">This machine</span>}
        <span className="ml-auto flex items-baseline gap-1.5 text-xs">
          <span className={statusText[status]}>{status}</span>
          {ago && <span className="text-neutral-500">· heard {ago}</span>}
        </span>
      </header>
      <p className="truncate px-4 font-mono text-[11px] text-neutral-600">{machine.id}</p>

      {sample ? (
        <div className={away ? "opacity-60 grayscale" : undefined}>
          <div className="flex items-end justify-between gap-3 px-4 pt-4">
            <div
              role="meter"
              aria-label="Load per core"
              aria-valuenow={sample.load_per_core}
              aria-valuemin={0}
              aria-valuemax={Math.max(1, sample.load_per_core)}
              className="flex items-baseline gap-1"
            >
              <span className={`text-3xl font-semibold tabular-nums ${away ? "text-neutral-400" : loadTone(sample.load_per_core)}`}>{sample.load_per_core.toFixed(2)}</span>
              <span className="text-sm text-neutral-500">load per core</span>
            </div>
            <span className="pb-1 text-[11px] text-neutral-600">{spanWords(covers)}</span>
          </div>
          <Spark
            points={points}
            value={(p) => p.load}
            top={Math.max(1.25, ...loads) * 1.08}
            guide={1}
            now={end}
            covers={covers}
            tone={away ? "text-neutral-500" : loadTone(sample.load_per_core)}
            label={loads.length ? `Load per core, ${spanWords(covers)}, from ${Math.min(...loads).toFixed(2)} to ${Math.max(...loads).toFixed(2)}` : "Load per core, no samples yet"}
            tall
          />
          <dl className="mt-3 grid grid-cols-3 border-y border-neutral-800 text-xs">
            <div className="flex flex-col gap-0.5 px-4 py-2.5">
              <dt className="text-neutral-500">Memory free</dt>
              <dd className="font-medium text-neutral-100 tabular-nums">{gb(sample.free_memory)}</dd>
              <Spark
                points={points}
                value={(p) => p.memory}
                top={Math.max(1, ...memories) * 1.15}
                now={end}
                covers={covers}
                tone="text-violet-400"
                label={memories.length ? `Free memory, ${spanWords(covers)}, from ${gb(Math.min(...memories))} to ${gb(Math.max(...memories))}` : "Free memory, no samples yet"}
              />
            </div>
            <div className="flex flex-col gap-0.5 border-l border-neutral-800 px-4 py-2.5">
              <dt className="text-neutral-500">Disk free</dt>
              <dd className="font-medium text-neutral-100 tabular-nums">{gb(sample.free_disk)}</dd>
            </div>
            <div className="flex flex-col gap-0.5 border-l border-neutral-800 px-4 py-2.5">
              <dt className="text-neutral-500">Sessions</dt>
              <dd className="font-medium text-neutral-100 tabular-nums">{sample.sessions}</dd>
            </div>
          </dl>
        </div>
      ) : (
        <p className="mx-4 mt-4 mb-1 rounded-lg border border-dashed border-neutral-800 px-3 py-4 text-center text-xs text-neutral-500">No sample yet</p>
      )}

      {away && (
        <p className="mx-4 mt-3 rounded-lg bg-red-400/10 px-3 py-2 text-xs text-red-300">
          {plural(all.length, "terminal")} unreachable until it is heard from. Its numbers are the last known.
        </p>
      )}

      <States glyphs={all.map((r) => r.glyph)} />

      <ul aria-label={`Workspaces on ${machine.name}`} className="grid grid-cols-[repeat(auto-fill,minmax(16rem,1fr))] gap-x-2 px-2 pt-1 pb-2">
        {machine.workspaces.map((workspace) => {
          const ordered = byUrgency(rows(workspace));
          const lead = ordered[0];
          const inner = (
            <>
              <span className="min-w-0 flex-1 truncate">
                <span className="font-medium">{workspace.name}</span>
                <span className="text-neutral-500"> · {workspace.project}</span>
              </span>
              <span aria-hidden="true" className="flex shrink-0 items-center gap-0.5">
                {ordered.slice(0, 6).map((r) => (
                  <StateGlyph key={r.terminal} glyph={r.glyph} />
                ))}
                {ordered.length > 6 && <span className="pl-0.5 text-[11px] text-neutral-500">+{ordered.length - 6}</span>}
              </span>
            </>
          );
          const row = "flex items-center gap-3 rounded-md px-2 py-1.5 text-sm text-neutral-200";
          return (
            <li key={workspace.id}>
              {lead ? (
                <Link to="/terminals/$terminalId" params={{ terminalId: lead.terminal }} className={`${row} hover:bg-neutral-800/80`}>
                  {inner}
                </Link>
              ) : (
                <span className={row}>{inner}</span>
              )}
            </li>
          );
        })}
        {machine.workspaces.length === 0 && <li className="px-2 py-1.5 text-xs text-neutral-500">No workspaces on this machine</li>}
      </ul>
    </article>
  );
}

/** The machine's terminals counted by state, most urgent first, over a bar of the same shares. */
function States({ glyphs }: { glyphs: Glyph[] }) {
  if (glyphs.length === 0) return null;
  const counts = urgency.map((glyph) => ({ glyph, n: glyphs.filter((g) => g === glyph).length })).filter((c) => c.n > 0);
  return (
    <div className="flex flex-col gap-2 px-4 pt-3 pb-1">
      <div aria-hidden="true" className="flex h-1.5 gap-0.5 overflow-hidden rounded-full">
        {counts.map(({ glyph, n }) => (
          <span key={glyph} className={stateBar[glyph]} style={{ flexGrow: n }} />
        ))}
      </div>
      <ul aria-label="Terminals by state" className="flex flex-wrap gap-x-3 gap-y-1 text-xs">
        {counts.map(({ glyph, n }) => (
          <li key={glyph} className={`flex items-center gap-1.5 ${glyphText[glyph]}`}>
            <StateGlyph glyph={glyph} still />
            {`${n} ${stateWord[glyph]}`}
          </li>
        ))}
      </ul>
    </div>
  );
}

/**
 * A line of one number over the last `covers` milliseconds, ending at `now` on the right, with the area
 * under it shaded and a dot on the newest point. `guide` draws a dashed line at that value.
 */
function Spark({
  points,
  value,
  top,
  now,
  covers,
  tone,
  label,
  guide,
  tall = false,
}: {
  points: Point[];
  value: (p: Point) => number;
  top: number;
  now: number;
  covers: number;
  tone: string;
  label: string;
  guide?: number;
  tall?: boolean;
}) {
  const h = 30;
  const xy = scaled(points, value, top, now, covers).map(([x, y]) => [x * 100, y * h] as const);
  const line = xy.map(([x, y]) => `${x.toFixed(2)},${y.toFixed(2)}`).join(" ");
  const last = xy.at(-1);
  const guideY = guide === undefined ? undefined : (1 - guide / top) * h;
  return (
    <div className={`relative ${tall ? "mx-4 mt-2 h-16" : "mt-1 h-6"} ${tone}`}>
      <svg role="img" aria-label={label} viewBox={`0 0 100 ${h}`} preserveAspectRatio="none" className="absolute inset-0 size-full overflow-visible">
        {guideY !== undefined && (
          <line x1="0" x2="100" y1={guideY} y2={guideY} className="stroke-neutral-700" strokeDasharray="3 3" vectorEffect="non-scaling-stroke" />
        )}
        {xy.length > 1 && (
          <>
            <polygon points={`${xy[0][0]},${h} ${line} ${last![0]},${h}`} fill="currentColor" opacity={0.12} />
            <polyline points={line} fill="none" stroke="currentColor" strokeWidth={1.5} strokeLinejoin="round" vectorEffect="non-scaling-stroke" />
          </>
        )}
      </svg>
      {guideY !== undefined && (
        <span className="pointer-events-none absolute left-0 -translate-y-full pb-0.5 text-[10px] text-neutral-600" style={{ top: `${(guideY / h) * 100}%` }}>
          1 per core
        </span>
      )}
      {last && (
        <span
          aria-hidden="true"
          className="pointer-events-none absolute size-1.5 -translate-x-1/2 -translate-y-1/2 rounded-full bg-current ring-2 ring-neutral-900"
          style={{ left: `${last[0]}%`, top: `${(last[1] / h) * 100}%` }}
        />
      )}
    </div>
  );
}
