import { useQuery } from "@tanstack/react-query";
import { createRootRoute, createRoute, createRouter, useNavigate, useParams, useRouterState } from "@tanstack/react-router";
import { useEffect, useLayoutEffect, useMemo, useState } from "react";
import { Board } from "./Board";
import { ChangesPanel } from "./ChangesPanel";
import { LinkBanner, Unreachable } from "./Connection";
import { useCreateWorkspace } from "./Composer";
import { JumpPalette } from "./JumpPalette";
import { Launchpad } from "./Launchpad";
import { Machines } from "./Machines";
import { NewSession } from "./NewSession";
import { NewTask } from "./NewTask";
import { Sidebar } from "./Sidebar";
import { StepsPanel, type StepsOf } from "./StepsPanel";
import { TabStrip } from "./TabStrip";
import { beside, follow, type Split } from "./split";
import { touched, withoutTab } from "./tabs";
import { TerminalHeader } from "./TerminalHeader";
import { TerminalPane } from "./TerminalPane";
import { TerminalActions } from "./TerminalMenu";
import { fleet, locate, rows, terminalRunning } from "./fleet";
import { useShortcut } from "./shortcut";
import { useBriefs } from "./useBriefs";
import { machineQuery } from "./api";
import { useFleet } from "./useFleet";
import { AlertsButton, useAlerts } from "./AlertsBell";

/**
 * The sidebar, and the terminal the URL names (`/terminals/$terminalId`) under its header filling
 * the rest. On a narrow screen the sidebar is a drawer: the board (or the machines view) is the
 * home, with a bar at the bottom that opens it, and an open terminal's header opens it too. Cmd+K (or Ctrl+K outside
 * the terminal, where the shell keeps it) opens the jump palette over everything.
 *
 * Above the header, the open workspace's terminals are tabs with their states, and a + starts
 * another session or shell in it. The few terminals shown last (and both sides of a split) keep
 * their pane attached behind the others, so going back to one is instant, from its tab or another
 * workspace; an older one attaches again when shown and gets its screen and scrollback back from
 * the holder.
 *
 * With nothing open, the board shows every workspace as a card with its live states and changes,
 * or, switched to its timeline, every session's turns on one clock; a turn opens its terminal
 * with the steps column.
 *
 * A fleet with no workspaces opens on one question, what Claude should work on, and a box to
 * answer it in: the task names the workspace and its branch, Start makes it and lands in its
 * session already at work, and the three states it will show are explained under it.
 *
 * The machines view (`/machines`, from the board's switch or a machine's name in the sidebar)
 * shows every machine as a card: its status, when it was last heard, its load drawn over the last
 * ten minutes from the samples this page has seen, its free memory and disk, its terminals by
 * state and its workspaces.
 *
 * A workspace is made one way, from a task box: the first run's, the board's top line, or the
 * New task dialog (the sidebar's button, or the palette with what was typed as the task). It is landed in: once its session shows up, its terminal opens, its setup shell a tab beside it.
 *
 * A workspace's changes since its base open as a sheet from the header or the workspace's row.
 *
 * The header's Steps button shows, in a column at the right (the whole screen on a narrow one),
 * every prompt, tool call and question of the session with the keyboard, turn by turn. It follows
 * whichever terminal is open until it is closed.
 *
 * Cmd+F (Ctrl+F outside the terminal) or the header's Find button finds text in the whole
 * scrollback of the terminal with the keyboard; the bar follows it as the steps column does.
 *
 * Each session is named by its first prompt, and a busy one says what it is doing, from the
 * daemon's briefs of its hooks.
 *
 * The browser tab shows the fleet's state in its title and icon, and the sidebar's bell turns on
 * desktop alerts for a session that finishes its turn or needs you.
 *
 * Right-clicking (or long-pressing) a terminal anywhere it is listed, the header's More button, or
 * Tab on it in the palette, lists what can be done with it: open, open beside, steps, changes, a
 * new session or shell in its workspace, and ending it. Ending a terminal that still
 * runs asks first; one that already ended is cleared at once, as are all of a workspace's ended
 * ones from its row. When the open terminal is gone, its neighbouring tab shows, or the board.
 *
 * On a wide screen a second terminal can be shown beside the open one (the header's split button,
 * Cmd+\ or Shift+Enter in the palette), each with its own header. The URL names the side with the
 * keyboard; pressing in the other side moves it there, and a terminal opened from anywhere takes
 * the side that has it. On a narrow screen only that side shows.
 *
 * Until the daemon has sent its state once, a failed try fills the page with where it looked and
 * when it tries next, never an empty fleet. Once it has, a lost link greys everything and stills
 * the working glyphs under a line that says since when, until the daemon sends again; then a line
 * says, once, that it is back.
 */
function Layout() {
  const { state, connected, samples, link } = useFleet();
  const { terminalId } = useParams({ strict: false });
  const onMachines = useRouterState({ select: (s) => s.location.pathname === "/machines" });
  const briefs = useBriefs(state.records);
  const { data: here } = useQuery(machineQuery);
  const traced = useMemo(() => new Set(Object.keys(briefs)), [briefs]);
  const machines = useMemo(() => fleet(state.records, state.settings, briefs), [state.records, state.settings, briefs]);
  const [drawer, setDrawer] = useState(false);
  /** The palette, and whether its picks open beside the open terminal. */
  const [jump, setJump] = useState<"jump" | "beside">();
  const [split, setSplit] = useState<Split>();
  /** The New task dialog: open, and the task it starts with. */
  const [create, setCreate] = useState({ open: false, task: "" });
  /** Whether the steps column is shown. */
  const [stepsOpen, setStepsOpen] = useState(false);
  /** The find bar over the terminal with the keyboard: 0 closed, and one more each time it is asked for. */
  const [finding, setFinding] = useState(0);
  /** The workspace whose changes are open. */
  const [changesOf, setChangesOf] = useState<string>();
  /** The workspace a new session is being asked for from a terminal's actions. */
  const [sessionIn, setSessionIn] = useState<{ id: string; name: string }>();
  /** The session of a workspace just made, until it shows up and is opened. */
  const [landing, setLanding] = useState<string>();
  /** The first-run page's create, held here so it outlives the page when its workspace arrives first. */
  const launch = useCreateWorkspace();
  const navigate = useNavigate();
  const located = terminalId ? locate(machines, terminalId) : undefined;
  const stepsOf: StepsOf | undefined = !located
    ? undefined
    : located.session
      ? { kind: "session", id: located.session, name: located.name, title: located.title, glyph: located.glyph, label: located.label }
      : { kind: "shell", name: located.name };

  const gone = useMemo(() => new Set(state.records.flatMap((r) => (r.kind === "terminal" && r.deleted ? [r.body.id] : []))), [state.records]);
  /** Terminals whose pane is attached, most recent first: the last few shown, and both sides of a split. */
  const [attached, setAttached] = useState<string[]>([]);
  /** The state each terminal had when it was last shown. */
  const [seen, setSeen] = useState<Record<string, string>>({});
  // Adjusted while rendering, so a terminal opened from anywhere is attached in the same paint.
  const [opened, setOpened] = useState<string>();
  if (terminalId !== opened) {
    setOpened(terminalId);
    const next = follow(split, opened, terminalId);
    setSplit(next);
    if (terminalId) setAttached(touched(attached, terminalId, next ? [next.left, next.right] : []));
  }
  if (terminalId && located && seen[terminalId] !== located.label) setSeen({ ...seen, [terminalId]: located.label });
  const tabs = located ? rows(located.workspace) : [];
  // The open workspace's tabs as last seen, so a terminal that goes knows its neighbours.
  const [lastTabs, setLastTabs] = useState<string[]>([]);
  if (located && tabs.map((t) => t.terminal).join() !== lastTabs.join()) setLastTabs(tabs.map((t) => t.terminal));
  // The open terminal was ended or archived: show its neighbouring tab, or the board.
  const [left, setLeft] = useState<{ from: string; to?: string }>();
  if (terminalId && gone.has(terminalId) && left?.from !== terminalId) {
    setLeft({ from: terminalId, to: withoutTab(lastTabs.filter((id) => id === terminalId || !gone.has(id)), terminalId).next });
  }
  useEffect(() => {
    if (left) void navigate(left.to ? { to: "/terminals/$terminalId", params: { terminalId: left.to } } : { to: "/" });
  }, [left, navigate]);
  if (attached.some((id) => id !== terminalId && gone.has(id))) setAttached(attached.filter((id) => id === terminalId || !gone.has(id)));
  if (split && (gone.has(split.left) || gone.has(split.right))) setSplit(undefined);
  const pair = split && terminalId && (split.left === terminalId || split.right === terminalId) ? split : undefined;

  // Adjusted while rendering, like the panes above: once the new session shows up, it is opened.
  const [landed, setLanded] = useState<string>();
  const arrived = landing ? machines.flatMap((m) => m.workspaces).find((w) => w.sessions.some((s) => s.id === landing)) : undefined;
  if (arrived) {
    const session = arrived.sessions.find((s) => s.id === landing)!;
    setLanding(undefined);
    setLanded(session.terminal);
  }
  useEffect(() => {
    if (landed) void navigate({ to: "/terminals/$terminalId", params: { terminalId: landed } });
  }, [landed, navigate]);

  const changed = new Set(tabs.flatMap((t) => (seen[t.terminal] !== undefined && seen[t.terminal] !== t.label ? [t.terminal] : [])));
  const show = (id: string) => void navigate({ to: "/terminals/$terminalId", params: { terminalId: id } });
  const alerts = useAlerts(machines, state.synced, show);
  /** Ends the split by closing side `id`; the other side keeps or takes the keyboard. */
  const closePane = (id: string) => {
    setSplit(undefined);
    if (pair && id === terminalId) show(id === pair.left ? pair.right : pair.left);
  };
  const openBeside = (id: string) => {
    if (!terminalId || id === terminalId) return;
    setSplit(beside(pair, terminalId, id));
    show(id);
  };
  /** Gives the keyboard to side `id` of the split when the user presses or tabs into it. */
  const focusPane = (id: string) => (event: React.SyntheticEvent) => {
    if (id === terminalId || (event.target instanceof Element && event.target.closest("button"))) return;
    show(id);
  };
  const close = () => setDrawer(false);
  const openJump = () => {
    setDrawer(false);
    setJump("jump");
  };
  const openChanges = (workspace: string) => {
    setDrawer(false);
    setChangesOf(workspace);
  };
  const openCreate = (task = "") => {
    setDrawer(false);
    setCreate({ open: true, task });
  };

  useLayoutEffect(() => {
    if (!drawer) return;
    const escape = (event: KeyboardEvent) => event.key === "Escape" && setDrawer(false);
    window.addEventListener("keydown", escape);
    return () => window.removeEventListener("keydown", escape);
  }, [drawer]);

  useShortcut("k", () => {
    setDrawer(false);
    setJump((open) => (open ? undefined : "jump"));
  });
  useShortcut("f", terminalId ? () => setFinding((n) => n + 1) : undefined);
  useShortcut("\\", terminalId ? () => (split ? setSplit(undefined) : setJump("beside")) : undefined);

  // Never had the daemon's state: say so, rather than show an empty fleet.
  if (!state.synced && link.lostAt !== undefined) return <Unreachable link={link} />;
  const stale = link.lostAt !== undefined;
  const workspaces = machines.flatMap((m) => m.workspaces);
  // A fleet with no workspaces opens on the first-run page, kept while its create is under way or
  // its workspace was made but its session failed, so what happened stays on screen.
  const firstRun = !terminalId && !onMachines && state.synced && (workspaces.length === 0 || launch.create.isPending || (launch.made && launch.create.isError));
  const board = !terminalId && !onMachines && !firstRun;

  // On a narrow screen the sidebar is a drawer over whatever is shown: the board, the machines view or a terminal.
  const place = drawer ? "fixed inset-0 z-30 flex md:static md:z-auto" : "hidden md:flex";
  return (
    <TerminalActions
      machines={machines}
      current={terminalId}
      onOpen={(id) => {
        setDrawer(false);
        show(id);
      }}
      onBeside={openBeside}
      onSteps={(id) => {
        setDrawer(false);
        setStepsOpen(true);
        show(id);
      }}
      onChanges={openChanges}
      onFind={() => setFinding((n) => n + 1)}
      onNewSession={(id, name) => {
        setDrawer(false);
        setSessionIn({ id, name });
      }}
    >
      <div className="flex h-dvh flex-col overflow-hidden bg-neutral-950 text-neutral-100">
        <LinkBanner link={link} />
        <div data-stale={stale || undefined} className="relative flex min-h-0 flex-1">
          <div className={`${place} shrink-0`}>
            <Sidebar
              machines={machines}
              onCreate={() => openCreate()}
              selected={terminalId}
              onNavigate={close}
              onClose={close}
              onJump={openJump}
              onChanges={openChanges}
              alerts={<AlertsButton {...alerts} />}
            />
          </div>
          <main className="flex min-w-0 flex-1 flex-col">
            <TabStrip
              workspace={located?.workspace}
              machine={located?.machine}
              active={terminalId}
              beside={pair && (pair.left === terminalId ? pair.right : pair.left)}
              changed={changed}
            />
            {terminalId && (
              <div className="flex shrink-0">
                {(pair ? [pair.left, pair.right] : [terminalId]).map((id) => (
                  <div key={id} className="contents" onPointerDown={focusPane(id)}>
                    <TerminalHeader
                      id={id}
                      located={id === terminalId ? located : locate(machines, id)}
                      drawerOpen={drawer}
                      onMenu={() => setDrawer(true)}
                      onJump={openJump}
                      onChanges={openChanges}
                      onSteps={id === terminalId ? () => setStepsOpen(!stepsOpen) : undefined}
                      stepsOpen={stepsOpen}
                      onFind={id === terminalId ? () => setFinding(finding ? 0 : 1) : undefined}
                      finding={finding > 0}
                      onSplit={pair ? undefined : () => setJump("beside")}
                      onClosePane={pair ? () => closePane(id) : undefined}
                      beside={id !== terminalId}
                    />
                  </div>
                ))}
              </div>
            )}
            <div className="relative min-h-0 flex-1">
              {/* In a fixed order, so a pane's element never moves when it is used. */}
              {[...attached].sort().map((id) => {
                const side = pair && (id === pair.left ? "left" : id === pair.right ? "right" : undefined);
                const place = side === "left" ? "inset-y-0 left-0 right-0 md:right-1/2" : side === "right" ? "inset-y-0 right-0 left-0 md:left-1/2 md:border-l md:border-neutral-800" : "inset-0";
                const seen = id === terminalId ? "" : side ? "invisible md:visible" : "invisible";
                const where = locate(machines, id);
                return (
                  <div key={id} data-testid={`pane-${id}`} onPointerDownCapture={side && focusPane(id)} onFocusCapture={side && focusPane(id)} className={`absolute ${place} ${seen}`}>
                    <TerminalPane
                      id={id}
                      running={terminalRunning(state, id)}
                      active={id === terminalId}
                      name={where?.name}
                      glyph={where?.glyph}
                      finding={id === terminalId ? finding : 0}
                      onFindClose={() => setFinding(0)}
                      here={!!where && where.machine.id === here}
                      workspace={where?.session ? where.workspace.id : undefined}
                    />
                  </div>
                );
              })}
              {!terminalId && onMachines && <Machines machines={machines} history={samples} here={here} connected={connected} onJump={openJump} onMenu={() => setDrawer(true)} />}
              {firstRun && <Launchpad projects={state.settings.projects} workspaces={workspaces} launch={launch} onCreated={setLanding} onMenu={() => setDrawer(true)} />}
              {board && state.synced && (
                <Board
                  machines={machines}
                  projects={state.settings.projects}
                  onJump={openJump}
                  onCreated={setLanding}
                  onChanges={openChanges}
                  traced={traced}
                  onMenu={() => setDrawer(true)}
                  onSteps={(id) => {
                    setStepsOpen(true);
                    show(id);
                  }}
                />
              )}
            </div>
          </main>
          {stepsOpen && stepsOf && <StepsPanel key={located?.session ?? terminalId} of={stepsOf} onClose={() => setStepsOpen(false)} />}
        </div>
        <ChangesPanel workspace={machines.flatMap((m) => m.workspaces).find((w) => w.id === changesOf)} onClose={() => setChangesOf(undefined)} />
        <JumpPalette
          machines={machines}
          open={jump !== undefined}
          onOpenChange={(open) => setJump(open ? "jump" : undefined)}
          onCreate={openCreate}
          current={terminalId && located ? { terminal: terminalId, title: `${located.workspace.name} / ${located.name}` } : undefined}
          besideOnly={jump === "beside"}
          onBeside={openBeside}
        />
        <NewTask
          projects={state.settings.projects}
          workspaces={workspaces}
          open={create.open}
          onOpenChange={(open) => setCreate((c) => ({ ...c, open }))}
          task={create.task}
          onCreated={setLanding}
        />
        {sessionIn && <NewSession key={sessionIn.id} workspace={sessionIn.id} name={sessionIn.name} onClose={() => setSessionIn(undefined)} onStarted={setLanding} />}
      </div>
    </TerminalActions>
  );
}

const rootRoute = createRootRoute({ component: Layout });
const indexRoute = createRoute({ getParentRoute: () => rootRoute, path: "/" });
const terminalRoute = createRoute({ getParentRoute: () => rootRoute, path: "/terminals/$terminalId" });
const machinesRoute = createRoute({ getParentRoute: () => rootRoute, path: "/machines" });

export const routeTree = rootRoute.addChildren([indexRoute, terminalRoute, machinesRoute]);
export const router = createRouter({ routeTree });

