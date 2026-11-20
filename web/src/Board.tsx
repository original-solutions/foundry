import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { ARCHIVE_ICON, TerminalMenu, useTerminalActions } from "./TerminalMenu";
import { useEffect, useId, useLayoutEffect, useRef, useState } from "react";
import { glance, lanes, topFiles, type Card, type Lane } from "./lanes";
import { useLeaving } from "./Archive";
import { changesQuery } from "./ChangesPanel";
import { splitPath, totals } from "./changes";
import type { MachineView } from "./fleet";
import { jumpKey, SearchIcon } from "./JumpPalette";
import type { Project } from "./api";
import { Composer } from "./Composer";
import { useCreateWorkspace } from "./CreateWorkspace";
import { plural, type Glyph } from "./labels";
import { glyphText, StateGlyph } from "./StateGlyph";
import { Named, Now } from "./Now";
import { pollEvery } from "./poll";
import { Timeline } from "./Timeline";

/** Which view of the fleet the home shows, kept for this browser. The machines view has its own URL. */
type View = "board" | "timeline";
const VIEW_KEY = "foundry.home";
function loadView(): View {
  try {
    return localStorage.getItem(VIEW_KEY) === "timeline" ? "timeline" : "board";
  } catch {
    return "board";
  }
}
/** Keeps `view` as the home's view for next time. */
export function saveView(view: View) {
  try {
    localStorage.setItem(VIEW_KEY, view);
  } catch {
    // Not kept: the view still changes.
  }
}

/**
 * How often a card on screen reads its workspace's changes while an agent in it works. A quiet
 * card is read when it comes on screen and when one of its terminals changes state, never on a
 * timer: each read runs git several times, and a fleet has dozens of workspaces.
 */
export const BOARD_BUSY_EVERY = 30_000;

/** A card shows this many terminals, then says how many more. */
const ROWS_SHOWN = 4;

const laneTitle: Record<Lane, string> = { needs: "Needs you", working: "Working", quiet: "Quiet" };
const laneTone: Record<Lane, string> = { needs: "text-amber-300", working: "text-sky-300", quiet: "text-neutral-400" };
const laneGlyph: Record<Lane, Glyph> = { needs: "waiting", working: "working", quiet: "ready" };

/** The card's edge, coloured by its most urgent terminal. */
const edge: Record<Glyph, string> = {
  waiting: "bg-amber-400",
  stuck: "bg-orange-400",
  failed: "bg-red-400",
  working: "bg-sky-400",
  starting: "bg-neutral-400",
  ready: "bg-emerald-400/50",
  done: "bg-neutral-600",
  away: "bg-neutral-700",
};

/**
 * The view with nothing open: every workspace in the fleet as a card, in lanes by what it needs
 * (you, its agents, or nothing), each with its terminals' live states and what it has changed
 * since its base. A card's terminals open from it; its changes open the changes sheet. A fleet with
 * no workspaces yet opens on the first-run page instead (`Launchpad`).
 *
 * At the top, a one-line box gives Claude its next task (`Composer`), beside the way to jump.
 * A switch beside the counts turns it into a timeline of every session's turns on one clock.
 *
 * On a phone it is the home: a line above the task box says what needs you, what works and what
 * rests, and a bar at the bottom (`PhoneNav`) takes the switch's place and opens the workspaces.
 */
export function Board({
  machines,
  projects,
  onJump,
  onChanges,
  onSteps,
  onCreated,
  onMore,
  traced,
  onMenu,
}: {
  machines: MachineView[];
  /** The fleet's projects, which a new task is started in. */
  projects: Project[];
  onJump: () => void;
  /** Called with the session a new task started, so the layout lands in it. */
  onCreated: (session: string) => void;
  /** Opens the full new-workspace dialog with this name. */
  onMore: (name: string) => void;
  onChanges: (workspace: string) => void;
  /** Opens a terminal with its steps column. */
  onSteps: (terminal: string) => void;
  /** The sessions this machine keeps steps for, which the timeline reads. */
  traced: Set<string>;
  /** Shows the workspaces, on a screen too narrow to keep them beside. */
  onMenu: () => void;
}) {
  const [view, setView] = useState(loadView);
  const navigate = useNavigate();
  const pick = (next: View | "machines") => {
    if (next === "machines") return void navigate({ to: "/machines" });
    setView(next);
    saveView(next);
  };
  const workspaces = machines.flatMap((m) => m.workspaces);
  const sessions = workspaces.reduce((n, w) => n + w.sessions.length, 0);
  const shells = workspaces.reduce((n, w) => n + w.shells.length, 0);
  const counts = `${plural(sessions, "session")} · ${plural(shells, "shell")} · ${plural(workspaces.length, "workspace")} · ${plural(machines.length, "machine")}`;
  const launch = useCreateWorkspace();
  const board = lanes(machines);
  const needs = board.find((l) => l.lane === "needs")?.cards.length ?? 0;

  const several = machines.length > 1;
  return (
    <div className="absolute inset-0 flex flex-col">
      <div className="min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto flex max-w-7xl flex-col gap-7 px-4 pt-4 pb-16 md:px-6 md:pt-6">
          <div className="flex flex-col gap-2">
            <p className={`flex items-center gap-2 px-1 pb-1 text-sm font-medium md:hidden ${needs ? "text-amber-300" : "text-neutral-400"}`}>
              <StateGlyph glyph={needs ? "waiting" : "ready"} still />
              {glance(board)}
            </p>
            <Composer mode="bar" id="board-task" projects={projects} workspaces={workspaces} launch={launch} onCreated={onCreated} onMore={onMore} onJump={onJump} doneLabel="Clear" />
            <div className="hidden items-center gap-3 px-1 md:flex">
              <p className="min-w-0 flex-1 text-xs text-neutral-500">{counts}</p>
              <ViewSwitch view={view} onPick={pick} />
            </div>
          </div>
          {view === "timeline" ? (
            <Timeline machines={machines} traced={traced} onSteps={onSteps} />
          ) : (
            board.map(({ lane, cards }) => (
              <LaneSection key={lane} lane={lane} count={cards.length}>
                {cards.map((card) => (
                  <WorkspaceCard key={card.workspace.id} card={card} showMachine={several} onChanges={onChanges} />
                ))}
              </LaneSection>
            ))
          )}
        </div>
      </div>
      <PhoneNav view={view} onPick={pick} needs={needs} onMenu={onMenu} />
    </div>
  );
}

function LaneSection({ lane, count, children }: { lane: Lane; count: number; children: React.ReactNode }) {
  const id = useId();
  return (
    <section aria-labelledby={id} className="flex flex-col gap-3">
      <div className="flex items-center gap-2 px-1">
        <span className={laneTone[lane]}>
          <StateGlyph glyph={laneGlyph[lane]} still />
        </span>
        <h2 id={id} className={`text-xs font-semibold tracking-wider uppercase ${laneTone[lane]}`}>
          {laneTitle[lane]}
        </h2>
        <span className="rounded-full bg-neutral-800 px-1.5 text-[11px] text-neutral-400">{count}</span>
        <span aria-hidden="true" className="h-px flex-1 bg-neutral-800" />
      </div>
      <div className="grid grid-cols-[repeat(auto-fill,minmax(17rem,1fr))] gap-3">{children}</div>
    </section>
  );
}

/** Whether the element is on screen, or near it; always true where the browser cannot tell. */
function useOnScreen<T extends Element>() {
  const ref = useRef<T>(null);
  const [onScreen, setOnScreen] = useState(() => typeof IntersectionObserver === "undefined");
  useLayoutEffect(() => {
    const element = ref.current;
    if (!element || typeof IntersectionObserver === "undefined") return;
    const observer = new IntersectionObserver((entries) => setOnScreen(entries[entries.length - 1].isIntersecting), { rootMargin: "200px" });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);
  return [ref, onScreen] as const;
}

function WorkspaceCard({ card, showMachine, onChanges }: { card: Card; showMachine: boolean; onChanges: (workspace: string) => void }) {
  const id = useId();
  const [ref, onScreen] = useOnScreen<HTMLElement>();
  const { workspace, machine, rows, lead } = card;
  const shown = rows.slice(0, ROWS_SHOWN);
  const terminals = useTerminalActions();
  return (
    <article
      ref={ref}
      aria-labelledby={id}
      className="group/card relative flex flex-col overflow-hidden rounded-xl border border-neutral-800 bg-neutral-900/60 transition-colors hover:border-neutral-700"
    >
      <span aria-hidden="true" className={`absolute inset-y-0 left-0 w-1 ${lead ? edge[lead.glyph] : "bg-neutral-800"}`} />
      <header className="flex flex-col gap-0.5 px-4 pt-3 pb-2">
        <div className="flex items-baseline gap-2">
          <h3 id={id} className="min-w-0 flex-1 truncate text-[15px] font-semibold text-neutral-100">
            {lead ? (
              <Link to="/terminals/$terminalId" params={{ terminalId: lead.terminal }} className="hover:underline hover:decoration-neutral-600 hover:underline-offset-4">
                {workspace.name}
              </Link>
            ) : (
              workspace.name
            )}
          </h3>
          {showMachine && (
            <span className="shrink-0 text-xs text-neutral-500" title={`Runs on ${machine.id}`}>
              {machine.name}
            </span>
          )}
          {terminals?.here === machine.id && (
            <button
              type="button"
              aria-label={`Archive ${workspace.name}`}
              title="Archive: see what stays and what stops, then stop its terminals and remove the worktree"
              onClick={() => terminals.archive(workspace.id)}
              className="-my-1 grid size-6 shrink-0 place-items-center self-center rounded text-neutral-500 hover:bg-neutral-800 hover:text-neutral-100 pointer-fine:opacity-0 pointer-fine:group-focus-within/card:opacity-100 pointer-fine:group-hover/card:opacity-100"
            >
              <svg viewBox="0 0 16 16" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                <path d={ARCHIVE_ICON} />
              </svg>
            </button>
          )}
        </div>
        <p className="truncate font-mono text-xs text-neutral-500">
          {workspace.project} · {workspace.branch}
        </p>
      </header>
      <ul className="flex flex-col px-2 pb-2">
        {shown.map((row) => (
          <li key={row.terminal}>
            <TerminalMenu id={row.terminal}>
              <Link
                to="/terminals/$terminalId"
                params={{ terminalId: row.terminal }}
                title={row.title ? `${row.name}: ${row.title}` : undefined}
                className={`flex items-start gap-2.5 rounded-md px-2 py-1 text-sm hover:bg-neutral-800/80 ${row.glyph === "done" || row.glyph === "away" ? "text-neutral-500" : "text-neutral-200"}`}
              >
                <span className="flex h-5 shrink-0 items-center">
                  <StateGlyph glyph={row.glyph} />
                </span>
                <span className="min-w-0 flex-1">
                  <span className="flex items-baseline gap-2 leading-5">
                    <Named name={row.name} title={row.title} />
                    <span className={`shrink-0 text-xs ${glyphText[row.glyph]}`}>{row.label}</span>
                  </span>
                  {row.now && <Now step={row.now} />}
                </span>
              </Link>
            </TerminalMenu>
          </li>
        ))}
        {rows.length > shown.length && <li className="px-2 py-1 text-xs text-neutral-500">+{rows.length - shown.length} more</li>}
        {rows.length === 0 && <li className="px-2 py-1 text-xs text-neutral-500">No terminals</li>}
      </ul>
      <ChangesFooter card={card} onScreen={onScreen} onOpen={() => onChanges(workspace.id)} />
    </article>
  );
}

/**
 * What the workspace changed since its base, read as the sheet reads it, but only while the card
 * is on screen: when it comes on screen, whenever one of its terminals changes state (so the end
 * of a turn shows what the turn wrote), and every `BOARD_BUSY_EVERY` while an agent in it works.
 * A state change off screen is read when the card comes back. A failed read says so instead of
 * the old numbers and is tried again on the timer; a workspace the daemon does not have (a 404)
 * is not asked again on a timer. Once its workspace is being archived, nothing is read or shown.
 */
function ChangesFooter({ card, onScreen, onOpen }: { card: Card; onScreen: boolean; onOpen: () => void }) {
  const busy = card.rows.some((r) => r.glyph === "working");
  const workspace = card.workspace.id;
  const client = useQueryClient();
  const leaving = useLeaving(workspace);
  const { data, error } = useQuery({
    ...changesQuery(workspace),
    enabled: onScreen && !leaving,
    staleTime: BOARD_BUSY_EVERY,
    retry: false,
    refetchInterval: pollEvery(BOARD_BUSY_EVERY, busy),
  });
  const states = card.rows.map((r) => `${r.terminal}:${r.glyph}`).join(" ");
  const last = useRef(states);
  useEffect(() => {
    if (last.current === states) return;
    last.current = states;
    if (leaving) return;
    // Read now if on screen; otherwise marked out of date, and read when it comes on screen.
    void client.invalidateQueries({ queryKey: changesQuery(workspace).queryKey, exact: true });
  }, [states, client, workspace, leaving]);

  let body: React.ReactNode;
  if (leaving) body = <span className="text-neutral-500">Archiving…</span>;
  else if (error) body = <span className="text-neutral-500">Changes not readable from here</span>;
  else if (!data) body = <span className="text-neutral-600">Reading changes…</span>;
  else if (data.files.length === 0 && data.commits.length === 0) body = <span className="text-neutral-500">No changes yet</span>;
  else {
    const t = totals(data);
    const top = topFiles(data, 3);
    const newest = data.commits.at(0);
    body = (
      <>
        <span className="flex flex-wrap items-center gap-x-2.5 gap-y-0.5 text-neutral-400">
          <span className="font-mono">
            <span className="text-emerald-400">+{t.added}</span> <span className={t.removed ? "text-red-400" : "text-neutral-600"}>−{t.removed}</span>
          </span>
          <span>{plural(t.files, "file")}</span>
          {t.uncommitted > 0 && <span className="text-amber-300">{t.uncommitted} uncommitted</span>}
          {data.commits.length > 0 && <span>{plural(data.commits.length, "commit")}</span>}
        </span>
        {top.names.length > 0 && (
          <span className="flex min-w-0 flex-wrap gap-1">
            {top.names.map((path) => (
              <span key={path} title={path} className="max-w-full truncate rounded bg-neutral-800/80 px-1.5 py-px font-mono text-[11px] text-neutral-300">
                {splitPath(path).name}
              </span>
            ))}
            {top.more > 0 && <span className="px-1 py-px text-[11px] text-neutral-500">+{top.more} more</span>}
          </span>
        )}
        {newest && (
          <span className="flex min-w-0 items-baseline gap-1.5">
            <span className="shrink-0 font-mono text-[11px] text-neutral-600">{newest.hash}</span>
            <span className="truncate text-neutral-300">{newest.subject}</span>
          </span>
        )}
      </>
    );
  }
  return (
    <button
      type="button"
      onClick={onOpen}
      aria-label={`Changes in ${card.workspace.name}`}
      title={error ? String(error.message) : "Changes since the base branch"}
      className="mt-auto flex flex-col gap-1.5 border-t border-neutral-800 bg-neutral-950/40 px-4 py-2.5 text-left text-xs transition-colors hover:bg-neutral-800/50"
    >
      {body}
    </button>
  );
}

/** The search bar at the top of the home views: a button that opens the jump palette. */
export function JumpBar({ onJump }: { onJump: () => void }) {
  return (
    <button
      type="button"
      onClick={onJump}
      className="group flex w-full items-center gap-3 rounded-xl border border-neutral-700/70 bg-neutral-900 px-4 py-3 text-left text-base text-neutral-400 shadow-lg shadow-black/40 transition-colors hover:border-neutral-500 hover:text-neutral-200"
    >
      <SearchIcon className="size-5 shrink-0 text-neutral-500 group-hover:text-neutral-300" />
      <span className="flex-1">Jump to a session or shell…</span>
      <kbd className="rounded border border-neutral-700 px-1.5 py-0.5 text-xs text-neutral-400">{jumpKey}</kbd>
    </button>
  );
}

const views = [
  { view: "board", name: "Board", icon: <BoardIcon /> },
  { view: "timeline", name: "Timeline", icon: <TimelineIcon /> },
  { view: "machines", name: "Machines", icon: <MachinesIcon /> },
] as const;

/** The switch between the home's views of the fleet: its workspaces, its turns and its machines. */
export function ViewSwitch({ view, onPick }: { view: View | "machines"; onPick: (view: View | "machines") => void }) {
  return (
    <div role="radiogroup" aria-label="View" className="flex shrink-0 gap-0.5 rounded-lg border border-neutral-800 bg-neutral-900 p-0.5 text-xs">
      {views.map((v) => (
        <button
          key={v.view}
          type="button"
          role="radio"
          aria-checked={view === v.view}
          onClick={() => onPick(v.view)}
          className={`flex items-center gap-1.5 rounded-md px-2.5 py-1 font-medium transition-colors ${view === v.view ? "bg-neutral-700 text-neutral-50" : "text-neutral-400 hover:text-neutral-200"}`}
        >
          {v.icon}
          {v.name}
        </button>
      ))}
    </div>
  );
}

/**
 * The home's bar at the bottom of a phone, in reach of a thumb: the switch's views, the board's
 * marked with how many workspaces need you, and the workspaces, which open as a drawer.
 */
export function PhoneNav({ view, onPick, needs, onMenu }: { view: View | "machines"; onPick: (view: View | "machines") => void; needs: number; onMenu: () => void }) {
  const item = "relative flex flex-col items-center gap-1 pt-2 pb-1.5 text-[11px] font-medium transition-colors";
  return (
    <nav aria-label="Home" className="grid shrink-0 grid-cols-4 border-t border-neutral-800 bg-neutral-900 pb-[env(safe-area-inset-bottom)] md:hidden">
      {views.map((v) => {
        const on = view === v.view;
        const count = v.view === "board" && needs > 0;
        return (
          <button key={v.view} type="button" aria-current={on ? "page" : undefined} onClick={() => onPick(v.view)} className={`${item} ${on ? "text-neutral-50" : "text-neutral-500 hover:text-neutral-200"}`}>
            {on && <span aria-hidden="true" className="absolute inset-x-6 top-0 h-0.5 rounded-full bg-neutral-100" />}
            <span className="relative [&>svg]:size-5">
              {v.icon}
              {count && (
                <span aria-hidden="true" className="absolute -top-1.5 -right-3 min-w-4 rounded-full bg-amber-400 px-1 text-center text-[10px] leading-4 font-semibold text-neutral-950">
                  {needs}
                </span>
              )}
            </span>
            <span>
              {v.name}
              {count && <span className="sr-only">, {needs} {needs === 1 ? "needs" : "need"} you</span>}
            </span>
          </button>
        );
      })}
      <button type="button" aria-controls="sidebar" onClick={onMenu} className={`${item} text-neutral-500 hover:text-neutral-200`}>
        <span className="[&>svg]:size-5">
          <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" aria-hidden="true">
            <path d="M2 4h12M2 8h12M2 12h12" />
          </svg>
        </span>
        <span>Workspaces</span>
      </button>
    </nav>
  );
}

function MachinesIcon() {
  return (
    <svg viewBox="0 0 16 16" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" aria-hidden="true">
      <rect x="2" y="3" width="12" height="4" rx="1" />
      <rect x="2" y="9" width="12" height="4" rx="1" />
      <path d="M4.5 5h.01M4.5 11h.01" />
    </svg>
  );
}

function BoardIcon() {
  return (
    <svg viewBox="0 0 16 16" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
      <rect x="2" y="2.5" width="5" height="11" rx="1" />
      <rect x="9" y="2.5" width="5" height="6.5" rx="1" />
    </svg>
  );
}

function TimelineIcon() {
  return (
    <svg viewBox="0 0 16 16" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" aria-hidden="true">
      <path d="M2 4h6M5 8h8M3 12h4" />
    </svg>
  );
}
