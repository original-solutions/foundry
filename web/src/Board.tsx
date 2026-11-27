import { useMutationState, useQuery } from "@tanstack/react-query";
import { Link, useNavigate, useRouterState } from "@tanstack/react-router";
import { ARCHIVE_ICON, TerminalMenu, useTerminalActions } from "./TerminalMenu";
import { Children, useId, useState } from "react";
import { glance, lanes, topFiles, type Card, type Lane } from "./lanes";
import { buttonShape, pressed, viewLook } from "./Modal";
import { StepChips } from "./NextSteps";
import { nextSteps } from "./next";
import { shipLanes, useStaged, type ShipLane } from "./staged";
import { useLeaving } from "./Archive";
import { changesQuery } from "./ChangesPanel";
import { ShipLine, shippingQuery } from "./Shipping";
import { splitPath, totals } from "./changes";
import { branchOf, type MachineView } from "./fleet";
import type { Project } from "./api";
import { FactoryMark } from "./FactoryMark";
import { Composer, useCreateWorkspace } from "./Composer";
import { plural, words, type Glyph } from "./labels";
import { glyphFill, glyphText, StateGlyph } from "./StateGlyph";
import { Named, Now } from "./Now";
import { reply } from "./reply";
import { Said } from "./Said";
import { BOARD_BUSY_EVERY } from "./poll";
import { useCardChanges, useOnScreen } from "./cardChanges";
import { LayoutSwitch, loadLayout, saveLayout, WorkspaceLine, type Layout } from "./BoardList";
import { Timeline } from "./Timeline";
import { Machines } from "./Machines";
import type { History } from "./samples";
import { setHome, useHome, type Home } from "./home";
import { readStored, store } from "./storage";
import { ProjectFace } from "./ProjectFace";

/** Which of the board and the timeline the home shows, kept for this browser (see `home.ts`). The machines view has its own URL. */
type View = Home;
/** Where the home's view is kept: this browser's ship view, from before it became a grouping, is read from it. */
const VIEW_KEY = "foundry.home";

/** How the board groups its cards, kept for this browser: by what they need, or by how far their work has shipped. */
type Group = "attention" | "shipping";
const GROUP_KEY = "foundry.group";
function loadGroup(): Group {
  const kept = readStored(GROUP_KEY);
  if (kept === "attention" || kept === "shipping") return kept;
  // The ship view this browser kept before it became the board's shipping grouping.
  return readStored(VIEW_KEY) === "ship" ? "shipping" : "attention";
}
/** Keeps `group` for next time; when not kept, the grouping still changes. */
function saveGroup(group: Group) {
  store(GROUP_KEY, group);
}

export { BOARD_BUSY_EVERY };

/** A card shows this many terminals, then says how many more. */
const ROWS_SHOWN = 4;

const laneTitle: Record<Lane, string> = { needs: "Needs you", working: "Working", quiet: "Quiet" };
const laneHint: Record<Lane, string> = { needs: "Asked you something, stuck or failed", working: "An agent is working or starting", quiet: "Nothing is happening" };
const laneTone: Record<Lane, string> = { needs: "text-amber-300", working: "text-sky-300", quiet: "text-neutral-400" };
const laneGlyph: Record<Lane, Glyph> = { needs: "waiting", working: "working", quiet: "ready" };

/**
 * The view with nothing open: every workspace in the fleet as a card, in lanes by what it needs
 * (you, its agents, or nothing), each with its terminals' live states and what it has changed
 * since its base. A card's terminals open from it; its changes open the changes sheet. A fleet with
 * no workspaces yet opens on the first-run page instead (`Launchpad`).
 *
 * At the top, a one-line box gives Claude its next task (`Composer`), beside the way to jump.
 * A switch beside the counts turns it into a timeline of every session's turns on one clock, or
 * the machines (`/machines`). The three views share this head, kept mounted, so it never moves and
 * a task half typed survives a change of view. Above the lanes, the cards can be grouped instead by how far each workspace's work
 * has gone on its way to main (`useStaged`), counted and summed; either way an idle session of this
 * machine is offered the step that takes its work further, under it on its card. Beside the
 * grouping, the cards can be drawn as a list instead, one line per workspace (`WorkspaceLine`).
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
  traced,
  onMenu,
  history,
  connected,
  onCreate,
}: {
  machines: MachineView[];
  /** The fleet's projects, which a new task is started in. */
  projects: Project[];
  onJump: () => void;
  /** Called with the session a new task started, so the layout lands in it, and the daemon's warning about its create, if any. */
  onCreated: (session: string, warning: string | null) => void;
  onChanges: (workspace: string) => void;
  /** Opens a terminal with its steps column. */
  onSteps: (terminal: string) => void;
  /** The sessions this machine keeps steps for, which the timeline reads. */
  traced: Set<string>;
  /** Shows the workspaces, on a screen too narrow to keep them beside. */
  onMenu: () => void;
  /** Each machine's samples since this page opened, which the machines view draws. */
  history: History;
  /** Whether the page hears the daemon now, so the machines view can say its numbers stopped. */
  connected: boolean;
  /** Opens the New task dialog set to a machine, from the machines view. */
  onCreate?: (machine: string) => void;
}) {
  const kept = useHome();
  const onMachines = useRouterState({ select: (s) => s.location.pathname === "/machines" });
  const view = onMachines ? "machines" : kept;
  const navigate = useNavigate();
  const pick = (next: View | "machines") => {
    if (next === "machines") return void navigate({ to: "/machines" });
    setHome(next);
    if (onMachines) void navigate({ to: "/" });
  };
  const workspaces = machines.flatMap((m) => m.workspaces);
  const sessions = workspaces.reduce((n, w) => n + w.sessions.length, 0);
  const shells = workspaces.reduce((n, w) => n + w.shells.length, 0);
  const offline = machines.filter((m) => m.status === "offline").length;
  const counts = [plural(sessions, "session"), plural(shells, "shell"), plural(workspaces.length, "workspace"), plural(machines.length, "machine"), ...(offline ? [`${offline} offline`] : [])].join(" · ");
  const launch = useCreateWorkspace();
  const board = lanes(machines);
  const needs = board.find((l) => l.lane === "needs")?.cards.length ?? 0;
  const [group, setGroup] = useState(loadGroup);
  const regroup = (next: Group) => {
    setGroup(next);
    saveGroup(next);
  };
  const [layout, setLayout] = useState(loadLayout);
  const relayout = (next: Layout) => {
    setLayout(next);
    saveLayout(next);
  };
  const Shown = layout === "list" ? WorkspaceLine : WorkspaceCard;
  const leaving = new Set(
    useMutationState({
      filters: { mutationKey: ["archive"], predicate: (m) => m.state.status === "pending" || (m.state.status === "success" && m.state.data === null) },
      select: (m) => String(m.options.mutationKey?.[1]),
    }),
  );
  const here = useTerminalActions()?.here;
  const staged = useStaged(
    board.flatMap((l) => l.cards),
    here,
    view === "board" && group === "shipping",
    leaving,
  );

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
            <Composer mode="bar" id="board-task" projects={projects} workspaces={workspaces} launch={launch} onCreated={(session, warning) => onCreated(session, warning)} onJump={onJump} doneLabel="Clear" />
            <div className="hidden items-center gap-3 px-1 md:flex">
              <p className="min-w-0 flex-1 text-xs text-neutral-500">{counts}</p>
              <ViewSwitch view={view} onPick={pick} />
            </div>
          </div>
          {view === "machines" ? (
            <Machines machines={machines} history={history} here={here} connected={connected} onCreate={onCreate} />
          ) : view === "timeline" ? (
            <Timeline machines={machines} traced={traced} onSteps={onSteps} />
          ) : (
            <>
              <GroupBar group={group} onPick={regroup} staged={staged} layout={layout} onLayout={relayout} />
              {group === "shipping"
                ? staged.lanes.map(({ lane, cards }) => {
                    const { name, hint, tone } = shipLanes.find((l) => l.lane === lane)!;
                    return (
                      <LaneSection key={lane} title={name} hint={hint} tone={tone} icon={<StageMark lane={lane} />} count={cards.length} list={layout === "list"}>
                        {cards.map((card) => (
                          <Shown key={`${card.workspace.id}:${lane}`} card={card} showMachine={several} onChanges={onChanges} arrived={staged.arrived[card.workspace.id] === lane} />
                        ))}
                      </LaneSection>
                    );
                  })
                : board.map(({ lane, cards }) => (
                    <LaneSection
                      key={lane}
                      title={laneTitle[lane]}
                      hint={laneHint[lane]}
                      tone={laneTone[lane]}
                      icon={<StateGlyph glyph={laneGlyph[lane]} still />}
                      count={cards.length}
                      list={layout === "list"}
                    >
                      {cards.map((card) => (
                        <Shown key={card.workspace.id} card={card} showMachine={several} onChanges={onChanges} />
                      ))}
                    </LaneSection>
                  ))}
            </>
          )}
        </div>
      </div>
      <PhoneNav view={view} onPick={pick} needs={needs} onMenu={onMenu} />
    </div>
  );
}

/**
 * The board's grouping, above its lanes: by what each workspace needs (attention), or by how far
 * its work has gone on its way to main (shipping), with the work summed while grouped that way.
 */
function GroupBar({
  group,
  onPick,
  staged,
  layout,
  onLayout,
}: {
  group: Group;
  onPick: (group: Group) => void;
  staged: ReturnType<typeof useStaged>;
  layout: Layout;
  onLayout: (layout: Layout) => void;
}) {
  const { sum, reading } = staged;
  // The work on its way to main, counted stage by stage as a scoreboard, then summed.
  return (
    <div className="-mb-3 flex flex-wrap items-center gap-x-3 gap-y-1.5 px-1">
      <div role="group" aria-label="Group cards by" className="flex shrink-0 items-center gap-1.5 text-xs text-neutral-500">
        <span className="pr-0.5">Group by</span>
        {(
          [
            ["attention", "Attention", "What each workspace needs: you, its agents, or nothing"],
            ["shipping", "Shipping", "How far each workspace's work has gone: only here, pushed, in review, merged"],
          ] as const
        ).map(([value, name, title]) => (
          <button key={value} type="button" title={title} aria-pressed={group === value} onClick={() => onPick(value)} className={`${buttonShape} ${pressed} border-neutral-700 text-neutral-300 hover:bg-neutral-800`}>
            {name}
          </button>
        ))}
      </div>
      {group === "shipping" && (
        <p className="min-w-0 text-xs text-neutral-400">
          {sum.workspaces === 0 ? (
            reading > 0 ? (
              "Reading where each workspace's work stands…"
            ) : (
              "No workspace has changed anything yet."
            )
          ) : (
            <>
              {shipLanes.slice(0, 4).map(({ lane, name, tone }, i) => {
                const count = staged.lanes.find((l) => l.lane === lane)?.cards.length ?? 0;
                return (
                  <span key={lane} className="whitespace-nowrap">
                    {i > 0 && <span className="px-1 text-neutral-600">›</span>}
                    <span className={`font-semibold tabular-nums ${count ? tone : "text-neutral-600"}`}>{count}</span> {name.toLowerCase()}
                  </span>
                );
              })}
              {" · "}
              <span className="font-mono">
                <span className="text-emerald-400">+{sum.added.toLocaleString()}</span> <span className={sum.removed ? "text-red-400" : "text-neutral-600"}>−{sum.removed.toLocaleString()}</span>
              </span>
              {" · "}
              {plural(sum.commits, "commit")}
              {reading > 0 && <span className="text-neutral-500"> · reading {reading} more…</span>}
            </>
          )}
        </p>
      )}
      <LayoutSwitch layout={layout} onPick={onLayout} />
    </div>
  );
}

/** A shipping lane's mark: a short bar in its colour, solid once the work left the machine. */
function StageMark({ lane }: { lane: ShipLane }) {
  const outline = lane === "here" || lane === "none" || lane === "unread";
  return <span aria-hidden="true" className={`block h-2.5 w-2.5 rounded-sm border-2 border-current ${outline ? "" : "bg-current"}`} />;
}

function LaneSection({
  title,
  hint,
  tone,
  icon,
  count,
  list = false,
  children,
}: {
  title: string;
  hint: string;
  tone: string;
  icon: React.ReactNode;
  count: number;
  /** Whether its workspaces are lines of one list rather than cards in a grid. */
  list?: boolean;
  children: React.ReactNode;
}) {
  const id = useId();
  return (
    <section aria-labelledby={id} className="flex flex-col gap-3">
      <div className="flex items-center gap-2 px-1" title={hint}>
        <span className={tone}>{icon}</span>
        <h2 id={id} className={`text-xs font-semibold tracking-wider uppercase ${tone}`}>
          {title}
        </h2>
        <span className="rounded-full bg-neutral-800 px-1.5 text-[11px] text-neutral-400">{count}</span>
        <span aria-hidden="true" className="h-px flex-1 bg-neutral-800" />
      </div>
      {list ? (
        <ul className="divide-y divide-neutral-800/80 overflow-hidden rounded-xl border border-neutral-800 bg-neutral-900/60">
          {Children.map(children, (child) => (
            <li>{child}</li>
          ))}
        </ul>
      ) : (
        <div className="grid grid-cols-[repeat(auto-fill,minmax(17rem,1fr))] gap-3">{children}</div>
      )}
    </section>
  );
}

function WorkspaceCard({ card, showMachine, onChanges, arrived = false }: { card: Card; showMachine: boolean; onChanges: (workspace: string) => void; arrived?: boolean }) {
  const id = useId();
  const [ref, onScreen] = useOnScreen<HTMLElement>();
  const { workspace, machine, rows, lead } = card;
  const shown = rows.slice(0, ROWS_SHOWN);
  const terminals = useTerminalActions();
  const leaving = useLeaving(workspace.id);
  // The session offered the next step: the first idle one, on this machine, which can type into it.
  const idle = terminals?.here === machine.id && !leaving ? rows.find((r) => r.session && r.glyph === "ready") : undefined;
  return (
    <article
      ref={ref}
      aria-labelledby={id}
      data-arrived={arrived || undefined}
      className={`group/card relative flex flex-col overflow-hidden rounded-xl border border-neutral-800 bg-neutral-900/60 transition-colors hover:border-neutral-700 ${arrived ? "motion-safe:animate-arrive" : ""}`}
    >
      <span aria-hidden="true" className={`absolute inset-y-0 left-0 w-1 ${lead ? glyphFill[lead.glyph] : "bg-neutral-800"}`} />
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
          {workspace.factory && <FactoryMark run={workspace.factory} />}
          {showMachine && (
            <span className="shrink-0 text-xs text-neutral-500" title={`Runs on ${machine.id}`}>
              {machine.name}
            </span>
          )}
          {terminals && (
            <button
              type="button"
              aria-label={`Archive ${workspace.name}`}
              title={words.archive}
              onClick={() => terminals.archive(workspace.id)}
              className="-my-1 grid size-6 shrink-0 place-items-center self-center rounded text-neutral-500 hover:bg-neutral-800 hover:text-neutral-100 pointer-fine:opacity-0 pointer-fine:group-focus-within/card:opacity-100 pointer-fine:group-hover/card:opacity-100"
            >
              <svg viewBox="0 0 16 16" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                <path d={ARCHIVE_ICON} />
              </svg>
            </button>
          )}
        </div>
        <p className="flex min-w-0 items-center gap-1.5 font-mono text-xs text-neutral-500">
          <ProjectFace project={workspace.project} />
          <span className="truncate">
            {workspace.project} · {branchOf(workspace)}
          </span>
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
            {row.said && <Said said={row.said} name={row.name} terminal={row.terminal} canReply={row.glyph === "ready"} />}
            {row === idle && <CardSteps key={rows.map((r) => r.glyph).join(" ")} workspace={workspace.id} row={row} />}
          </li>
        ))}
        {rows.length > shown.length && <li className="px-2 py-1 text-xs text-neutral-500">+{rows.length - shown.length} more</li>}
        {rows.length === 0 && <li className="px-2 py-1 text-xs text-neutral-500">No terminals</li>}
      </ul>
      {terminals?.here === machine.id ? (
        <>
          <ChangesFooter card={card} onScreen={onScreen} onOpen={() => onChanges(workspace.id)} />
          <ShipLine
            workspace={workspace.id}
            name={workspace.name}
            enabled={onScreen && !leaving}
            onArchive={() => terminals.archive(workspace.id)}
            className="border-t border-neutral-800/70 bg-neutral-950/40 px-4 py-2"
          />
        </>
      ) : (
        // Another machine's workspace: nothing of it is read from here, but its archive shows.
        leaving && <p className="mt-auto border-t border-neutral-800 bg-neutral-950/40 px-4 py-2.5 text-xs text-neutral-500">Archiving…</p>
      )}
    </article>
  );
}

/**
 * The step that takes the workspace's work further (commit, push and open a pull request, fix
 * failing checks, answer a review), offered to its idle session `row` and typed into its terminal.
 * Drawn from the reads the card's footer and pull request line already made; it asks for nothing.
 */
function CardSteps({ workspace, row }: { workspace: string; row: Card["rows"][number] }) {
  const { data: changes } = useQuery({ ...changesQuery(workspace), enabled: false });
  const { data: shipping } = useQuery({ ...shippingQuery(workspace), enabled: false });
  const steps = changes ? nextSteps(changes, shipping) : [];
  if (steps.length === 0) return null;
  return (
    <div className="pt-1 pb-1.5 pl-9">
      <StepChips steps={steps} name={row.name} send={(prompt) => reply(row.terminal, prompt)} look="card" />
    </div>
  );
}

/**
 * What the workspace changed since its base, at the foot of its card, read as `useCardChanges` says.
 * Only a card of this machine's workspace has one: the daemon serving the UI reads no other machine's.
 */
function ChangesFooter({ card, onScreen, onOpen }: { card: Card; onScreen: boolean; onOpen: () => void }) {
  const { data, error, leaving } = useCardChanges(card, onScreen);

  let body: React.ReactNode;
  if (leaving) body = <span className="text-neutral-500">Archiving…</span>;
  else if (error) body = <span className="text-neutral-500">{words.unreadable}</span>;
  else if (!data) body = <span className="text-neutral-600">Reading changes…</span>;
  else if (data.files.length === 0 && data.commits.length === 0) body = <span className="text-neutral-500">{words.noChanges}</span>;
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
      title={error ? String(error.message) : words.changes}
      className="mt-auto flex flex-col gap-1.5 border-t border-neutral-800 bg-neutral-950/40 px-4 py-2.5 text-left text-xs transition-colors hover:bg-neutral-800/50"
    >
      {body}
    </button>
  );
}

const views = [
  { view: "board", name: "Board", icon: <BoardIcon /> },
  { view: "timeline", name: "Timeline", icon: <TimelineIcon /> },
  { view: "machines", name: "Machines", icon: <MachinesIcon /> },
] as const;

/** The switch between the home's views of the fleet: its workspaces, its turns and its machines. */
function ViewSwitch({ view, onPick }: { view: View | "machines"; onPick: (view: View | "machines") => void }) {
  return (
    <div role="radiogroup" aria-label="View" className="flex shrink-0 gap-0.5 rounded-lg border border-neutral-800 bg-neutral-900 p-0.5 text-xs">
      {views.map((v) => (
        <button
          key={v.view}
          type="button"
          role="radio"
          aria-checked={view === v.view}
          onClick={() => onPick(v.view)}
          className={`flex items-center gap-1.5 rounded-md px-2.5 py-1 font-medium transition-colors ${view === v.view ? viewLook.on : viewLook.off}`}
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
function PhoneNav({ view, onPick, needs, onMenu }: { view: View | "machines"; onPick: (view: View | "machines") => void; needs: number; onMenu: () => void }) {
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

export function MachinesIcon() {
  return (
    <svg viewBox="0 0 16 16" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" aria-hidden="true">
      <rect x="2" y="3" width="12" height="4" rx="1" />
      <rect x="2" y="9" width="12" height="4" rx="1" />
      <path d="M4.5 5h.01M4.5 11h.01" />
    </svg>
  );
}

export function BoardIcon() {
  return (
    <svg viewBox="0 0 16 16" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
      <rect x="2" y="2.5" width="5" height="11" rx="1" />
      <rect x="9" y="2.5" width="5" height="6.5" rx="1" />
    </svg>
  );
}

export function TimelineIcon() {
  return (
    <svg viewBox="0 0 16 16" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" aria-hidden="true">
      <path d="M2 4h6M5 8h8M3 12h4" />
    </svg>
  );
}
