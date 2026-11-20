import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { ApiError, sessionTrail, type Mark } from "./api";
import type { Glyph } from "./labels";
import { StateGlyph } from "./StateGlyph";
import { ago, kindOf, tally, took, turns, type StepKind, type Turn } from "./trail";

/** How often an open panel reads a busy session's trail again. */
export const STEPS_EVERY = 2000;

/** What the panel shows the steps of: a session, or a shell, which has none. */
export type StepsOf = { kind: "session"; id: string; name: string; title?: string; glyph: Glyph; label: string } | { kind: "shell"; name: string };

const busy: Glyph[] = ["starting", "working", "waiting", "stuck"];

const chip: Record<StepKind, { mark: string; tone: string }> = {
  command: { mark: "$", tone: "bg-emerald-400/15 text-emerald-300" },
  edit: { mark: "±", tone: "bg-amber-400/15 text-amber-300" },
  read: { mark: "R", tone: "bg-neutral-700/60 text-neutral-300" },
  search: { mark: "S", tone: "bg-violet-400/15 text-violet-300" },
  web: { mark: "W", tone: "bg-sky-400/15 text-sky-300" },
  agent: { mark: "A", tone: "bg-violet-400/15 text-violet-300" },
  ask: { mark: "?", tone: "bg-amber-400/20 text-amber-300" },
  other: { mark: "·", tone: "bg-neutral-700/60 text-neutral-400" },
};

/**
 * The open session's story, beside its terminal: one card per turn, from the prompt that started
 * it through every tool it ran and question it asked to the end of the turn, each with how long
 * ago. Earlier turns fold to their prompt, what they did ("3 commands · 1 edit") and how long they
 * took; the newest stays open and grows live while the session works, keeping its newest step in
 * view. On a wide screen it is a column at the right; on a narrow one it takes the screen.
 */
export function StepsPanel({ of, onClose }: { of: StepsOf; onClose: () => void }) {
  return (
    <aside aria-label="Steps" className="fixed inset-0 z-30 flex flex-col bg-neutral-950 text-sm text-neutral-100 md:static md:z-auto md:w-[22rem] md:shrink-0 md:border-l md:border-neutral-800 motion-safe:md:animate-sheet-in">
      <header className="flex h-11 shrink-0 items-center gap-2 border-b border-neutral-800 bg-neutral-900 px-3 md:h-9 md:bg-neutral-950">
        <StepsIcon className="size-4 text-neutral-400" />
        <h2 className="font-medium">Steps</h2>
        <span className="min-w-0 flex-1 truncate text-neutral-400" title={of.kind === "session" ? of.title : undefined}>
          {of.name}
          {of.kind === "session" && of.title && <span className="text-neutral-500"> · {of.title}</span>}
        </span>
        <button type="button" aria-label="Close steps" onClick={onClose} className="-mr-1 rounded p-1.5 text-neutral-400 hover:bg-neutral-800 hover:text-neutral-100">
          <svg viewBox="0 0 16 16" className="size-4" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
            <path d="M4 4l8 8M12 4l-8 8" strokeLinecap="round" />
          </svg>
        </button>
      </header>
      {of.kind === "shell" ? <Note>A shell has no hooks, so it has no steps to show. Open a session to follow what it does.</Note> : <Trail session={of} />}
    </aside>
  );
}

function Note({ children }: { children: React.ReactNode }) {
  return <p className="p-4 text-neutral-500">{children}</p>;
}

function Trail({ session }: { session: Extract<StepsOf, { kind: "session" }> }) {
  const live = busy.includes(session.glyph);
  const { data, error, isPending } = useQuery({
    queryKey: ["trail", session.id, session.label],
    queryFn: () => sessionTrail(session.id),
    placeholderData: keepPreviousData,
    refetchInterval: (q) => (live && q.state.status !== "error" ? STEPS_EVERY : false),
    retry: false,
  });
  const now = useNow();
  const scroller = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);
  const count = data?.length ?? 0;
  // Keeps the newest step in view as steps arrive, unless the reader has scrolled up.
  useLayoutEffect(() => {
    const el = scroller.current;
    if (el && pinned.current) el.scrollTop = el.scrollHeight;
  }, [count, session.id]);

  if (error) {
    const elsewhere = error instanceof ApiError && error.status === 404;
    return <Note>{elsewhere ? "Steps are kept by the machine that runs it, and that is not this one, or its daemon restarted since it started." : error.message}</Note>;
  }
  if (isPending) return <Note>Reading steps…</Note>;
  const all = turns(data);
  if (all.length === 0) return <Note>No steps yet. Each prompt you send, tool it runs and question it asks shows here as it happens.</Note>;
  return (
    <div
      ref={scroller}
      onScroll={(e) => {
        const el = e.currentTarget;
        pinned.current = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
      }}
      className="min-h-0 flex-1 overflow-y-auto p-3"
    >
      <ol className="space-y-2">
        {all.map((turn, i) => (
          <TurnCard key={`${turn.start}-${i}`} turn={turn} newest={i === all.length - 1} session={session} now={now} />
        ))}
      </ol>
    </div>
  );
}

function TurnCard({ turn, newest, session, now }: { turn: Turn; newest: boolean; session: Extract<StepsOf, { kind: "session" }>; now: number }) {
  const [open, setOpen] = useState(newest);
  const [wasNewest, setWasNewest] = useState(newest);
  // A turn that stops being the newest folds, so only the one in progress stays open.
  if (wasNewest !== newest) {
    setWasNewest(newest);
    setOpen(newest);
  }
  const running = newest && turn.end === undefined && busy.includes(session.glyph);
  const heading = turn.prompt ?? "Earlier steps";
  const span = turn.end !== undefined ? took(turn.end - turn.start) : running ? took(now - turn.start) : undefined;
  return (
    <li aria-label={heading} className={`rounded-lg border bg-neutral-900/60 ${running ? "border-sky-400/30" : "border-neutral-800"}`}>
      <button type="button" aria-expanded={open} onClick={() => setOpen(!open)} className="flex w-full items-start gap-2 rounded-lg px-3 py-2 text-left hover:bg-neutral-800/50">
        <svg viewBox="0 0 16 16" className={`mt-0.5 size-3.5 shrink-0 text-neutral-500 transition-transform ${open ? "rotate-90" : ""}`} fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
          <path d="M6 4l4 4-4 4" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
        <span className="min-w-0 flex-1">
          <span className={`line-clamp-2 ${turn.prompt ? "text-neutral-100" : "text-neutral-400 italic"}`}>{turn.prompt ? `“${turn.prompt}”` : heading}</span>
          <span className="mt-0.5 flex flex-wrap gap-x-2 text-xs text-neutral-500">
            <span>{tally(turn.steps)}</span>
            {span && <span className="font-mono">{span}</span>}
            <span className="ml-auto">{ago(turn.start, now)}</span>
          </span>
        </span>
      </button>
      {open && (
        <div className="px-3 pb-2.5">
          {turn.steps.length > 0 && (
            <ol aria-label="Steps of this turn" className="ml-1.5 border-l border-neutral-800 pl-3">
              {turn.steps.map((step, i) => (
                <StepRow key={i} step={step} now={now} />
              ))}
            </ol>
          )}
          <End turn={turn} running={running} newest={newest} session={session} />
        </div>
      )}
    </li>
  );
}

function StepRow({ step, now }: { step: Mark; now: number }) {
  const kind = kindOf(step);
  const look = chip[kind];
  const text = kind === "command" ? step.text.slice(2) : step.text;
  return (
    <li className="relative flex items-start gap-2 py-0.5">
      <span aria-hidden="true" className={`mt-px grid size-4 shrink-0 place-items-center rounded font-mono text-[10px] leading-none font-semibold ${look.tone}`}>
        {look.mark}
      </span>
      <span className={`min-w-0 flex-1 text-xs leading-5 break-words ${kind === "ask" ? "text-amber-300" : "font-mono text-[11px] text-neutral-300"}`}>
        {text}
      </span>
      <span className="shrink-0 text-[11px] leading-5 text-neutral-600">{ago(step.at, now)}</span>
    </li>
  );
}

function End({ turn, running, newest, session }: { turn: Turn; running: boolean; newest: boolean; session: Extract<StepsOf, { kind: "session" }> }) {
  if (turn.end !== undefined) {
    return (
      <p className="mt-1.5 flex items-center gap-1.5 text-xs text-emerald-300">
        <StateGlyph glyph="done" still />
        Finished in {took(turn.end - turn.start)}
      </p>
    );
  }
  if (running) {
    return (
      <p className="mt-1.5 flex items-center gap-1.5 text-xs text-neutral-300">
        <StateGlyph glyph={session.glyph} />
        {session.label}
      </p>
    );
  }
  return newest ? null : <p className="mt-1.5 text-xs text-neutral-500">Ended without finishing its turn</p>;
}

/** The time now, again every `every` ms, so "how long ago" keeps up. */
export function useNow(every = 5000): number {
  const [now, setNow] = useState(Date.now);
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), every);
    return () => clearInterval(id);
  }, [every]);
  return now;
}

export function StepsIcon({ className = "" }: { className?: string }) {
  return (
    <svg viewBox="0 0 16 16" className={className} fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
      <circle cx="4" cy="3.5" r="1.25" />
      <circle cx="4" cy="8" r="1.25" />
      <circle cx="4" cy="12.5" r="1.25" />
      <path d="M7.5 3.5h6M7.5 8h6M7.5 12.5h4" strokeLinecap="round" />
    </svg>
  );
}
