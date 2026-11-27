import { useQuery } from "@tanstack/react-query";
import { createRootRoute, createRoute, createRouter, useNavigate, useParams, useRouterState } from "@tanstack/react-router";
import { useEffect, useLayoutEffect, useMemo, useState } from "react";
import { Board } from "./Board";
// The extension is spelled out: on a case-insensitive disk (macOS) "./Factory" finds factory.ts first.
import { Factory } from "./Factory.tsx";
import { FactoryContext } from "./factory";
import { ChangesPanel } from "./ChangesPanel";
import { LinkBanner, Unreachable } from "./Connection";
import { useCreateWorkspace } from "./Composer";
import { JumpPalette } from "./JumpPalette";
import { useCommands } from "./commands";
import { Launchpad } from "./Launchpad";
import { NewSession } from "./NewSession";
import { NewTask } from "./NewTask";
import { Sidebar } from "./Sidebar";
import { StepsPanel, type StepsOf } from "./StepsPanel";
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
import { Announcer, NotifyButton, useAlerts } from "./AlertsBell";
import { KeySheet, useHelpKey } from "./KeySheet";
import { Away } from "./Away";
import { Toasts } from "./Toasts";
import { Rail } from "./Rail";
import { loadRail, saveRail } from "./collapsed";
import { useMemory } from "./memory";
import { MD, useMediaQuery } from "./media";
import { AttentionList } from "./attention/AttentionList";
import { WaitingList } from "./placement/WaitingList";
import { upNext } from "./needing";

/**
 * The sidebar, and the terminal the URL names (`/terminals/$terminalId`) under its header filling
 * the rest. On a narrow screen the sidebar is a drawer: the board (or the machines view) is the
 * home, with a bar at the bottom that opens it, and an open terminal's header opens it too. Cmd+K (or Ctrl+K outside
 * the terminal, where the shell keeps it) opens the jump palette over everything: every terminal,
 * and every view and action of the app, so the factory and the timeline are reached from a phone too.
 *
 * On a wide screen the sidebar collapses to a rail as narrow as a button (its button, or Cmd+B),
 * which still shows every terminal's state as a column of glyphs per workspace; the choice is kept
 * in this browser, and with none a window too narrow for the list beside a terminal starts so.
 *
 * One bar heads the terminal: its workspace, then the workspace's terminals as tabs with their
 * states, a + that starts another session or shell in it, and, when another terminal needs you,
 * the way to the next one (Cmd+J from anywhere; the board's needs-you lane starts it too). Short
 * of room, it gives up what matters least first, keeping the workspace and the open tab. The
 * few terminals shown last (and both sides of a split) keep their pane attached behind the
 * others, so going back to one is instant, from its tab or another workspace; an older one
 * attaches again when shown and gets its screen and scrollback back from the holder.
 *
 * With nothing open, the board shows every workspace as a card with its live states and changes
 * (or as one line of a list, for a large fleet), or, switched to its timeline, every session's
 * turns on one clock, with the stretches each waited on you striped and added up in a row over the
 * lanes; a turn opens its terminal with the steps column.
 *
 * A fleet with no workspaces opens on one question, what Claude should work on, and a box to
 * answer it in: the task names the workspace and its branch, Start makes it and lands in its
 * session already at work, and the three states it will show are explained under it.
 *
 * The machines view (`/machines`, from the board's switch or a machine's name in the sidebar)
 * shows every machine as a card: its status, when it was last heard, its load drawn over the last
 * ten minutes from the samples this page has seen, its free memory and disk, its terminals by
 * state and its workspaces. It is headed by the machine a task Foundry places would go to now,
 * each card says whether it has room, and either opens the New task dialog set to a machine.
 *
 * The Factory view (`/factory`, from the rail's factory link) is headed by the line: how many
 * issues stand ready, building, held up and finished across the projects that are on, with the
 * fleet's factory settings folded to one line under it. Each project has its switch, login and
 * pause, its lease holder and generation or why it is idle, and its issues in the line's four
 * stations side by side: the ready queue, live runs with a Stop that asks first, runs held up, and
 * the last finished. It takes the place of the board.
 *
 * A workspace is made one way, from a task box: the first run's, the board's top line, or the
 * New task dialog (the sidebar's button, or the palette with what was typed as the task). It is landed in: once its session shows up, its terminal opens, its setup shell a tab beside it.
 * A task written as a list can be split into a workspace per item; those are not landed in.
 *
 * A workspace's changes since its base open as a sheet from the header or the workspace's row, each
 * diff coloured as code in its file's language, with the words a changed line changed marked.
 *
 * The header's Steps button shows, in a column at the right (the whole screen on a narrow one),
 * every prompt, tool call and question of the session with the keyboard, turn by turn, or, switched
 * to its files, every file it read or changed and every command it ran, each once; an edit opens
 * its file's diff in the changes sheet. It follows whichever terminal is open until it is closed.
 *
 * Cmd+F (Ctrl+F outside the terminal) or the header's Find button finds text in the whole
 * scrollback of the terminal with the keyboard; the bar follows it as the steps column does.
 *
 * Each session is named by its first prompt, and a busy one says what it is doing, from the
 * daemon's briefs of its hooks.
 *
 * The browser tab shows the fleet's state in its title and icon. A session you are not looking at
 * that finishes its turn or needs you is told once: as a card at the top right while you are at
 * the page, or as a desktop alert while you are not; coming back after a while, a catch-up card
 * heads the same stack. The sidebar's bell sets all three in one place; a screen reader hears the
 * news whatever it is set to. On a narrow screen what needs a person and the creates waiting for a
 * machine head the page instead, folded, above an open terminal's header, so they never lie over it.
 *
 * ? (outside a field or the terminal) or the sidebar's Keyboard button lists every key the app
 * answers. The sidebar's list is one Tab stop that the arrows walk.
 *
 * Right-clicking (or long-pressing) a terminal anywhere it is listed, the header's More button, or
 * Tab on it in the palette, lists what can be done with it: open, open beside, steps, changes, a
 * new session or shell in its workspace, and ending it. Ending a terminal that still
 * runs asks first; one that already ended is cleared at once, as are all of a workspace's ended
 * ones from its row. When the open terminal is gone, its neighbouring tab shows, or the board.
 *
 * On a wide screen a second terminal can be shown beside the open one (the header's split button,
 * Cmd+\ or Shift+Enter in the palette), each with its own bar and tabs. The URL names the side with the
 * keyboard; pressing in the other side moves it there, and a terminal opened from anywhere takes
 * the side that has it. On a narrow screen only that side shows.
 *
 * A load paints at once the fleet this browser last saw, greyed with its working glyphs still,
 * and it comes alive when the daemon sends its state; a line says it is connecting only after a
 * second, and, if a try fails, that the daemon cannot be reached. With nothing remembered, a failed
 * try fills the page with where it looked and when it tries next, never an empty fleet. Once the
 * daemon has sent, a lost link greys everything and stills the working glyphs under a line that
 * says since when, until the daemon sends again; then a line says, once, that it is back.
 */
function Layout() {
  const { state: sent, connected, samples, link } = useFleet();
  // Until the daemon's first state, the fleet this browser last saw stands in for it, frozen.
  const memory = useMemory(sent, link.lostAt === undefined);
  const remembering = !sent.synced && memory !== undefined;
  const state = remembering ? { records: memory.records, settings: memory.settings, synced: true } : sent;
  const { terminalId } = useParams({ strict: false });
  const onMachines = useRouterState({ select: (s) => s.location.pathname === "/machines" });
  const onFactory = useRouterState({ select: (s) => s.location.pathname === "/factory" });
  const briefs = useBriefs(state.records);
  const { data: here } = useQuery(machineQuery);
  const traced = useMemo(() => new Set(Object.keys(briefs)), [briefs]);
  const machines = useMemo(() => fleet(state.records, state.settings, briefs, state.online, here), [state.records, state.settings, briefs, state.online, here]);
  const [drawer, setDrawer] = useState(false);
  /** Whether the sidebar is collapsed to the rail on a wide screen. */
  const [rail, setRail] = useState(() => loadRail(window.innerWidth));
  const narrow = useNarrow();
  const flipRail = () => {
    saveRail(!rail);
    setRail(!rail);
  };
  /** The palette, and whether its picks open beside the open terminal. */
  const [jump, setJump] = useState<"jump" | "beside">();
  const [split, setSplit] = useState<Split>();
  /** The New task dialog: open, and the task and machine it starts with. */
  const [create, setCreate] = useState({ open: false, task: "", machine: "" });
  /** Whether the steps column is shown. */
  const [stepsOpen, setStepsOpen] = useState(false);
  /** The find bar over the terminal with the keyboard: 0 closed, and one more each time it is asked for. */
  const [finding, setFinding] = useState(0);
  /** The workspace whose changes are open. */
  const [changesOf, setChangesOf] = useState<{ workspace: string; file?: string }>();
  /** The workspace a new session is being asked for from a terminal's actions. */
  const [sessionIn, setSessionIn] = useState<{ id: string; name: string }>();
  /** The session of a workspace just made, until it shows up and is opened. */
  const [landing, setLanding] = useState<string>();
  /** The daemon's warning about the last task started from the board's box or the first-run page, until it is dismissed. */
  const [taskWarning, setTaskWarning] = useState<string>();
  /** Lands in a task's session, keeping the warning about its create, or clearing the last one when it has none. */
  const started = (session: string, warning: string | null) => {
    setLanding(session);
    setTaskWarning(warning ?? undefined);
  };
  /** The first-run page's create, held here so it outlives the page when its workspace arrives first. */
  const launch = useCreateWorkspace();
  const navigate = useNavigate();
  const located = terminalId ? locate(machines, terminalId) : undefined;
  const stepsOf: StepsOf | undefined = !located
    ? undefined
    : located.session
      ? { kind: "session", id: located.session, name: located.name, title: located.title, glyph: located.glyph, label: located.label, workspace: located.workspace.id }
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

  // Any workspace's terminals, so each side of a split marks its own tabs.
  const changed = new Set(
    machines.flatMap((m) => m.workspaces.flatMap((w) => rows(w).flatMap((t) => (seen[t.terminal] !== undefined && seen[t.terminal] !== t.label ? [t.terminal] : [])))),
  );
  const show = (id: string) => void navigate({ to: "/terminals/$terminalId", params: { terminalId: id } });
  // News is told from the daemon's own states only, never from the change between the remembered fleet and them.
  const { spoken, news, ...notify } = useAlerts(machines, sent.synced, show);
  /** The sessions the catch-up card lists, whose news it already shows. */
  const [caughtUp, setCaughtUp] = useState<string[]>([]);
  /** Whether the list of keys is open. */
  const [keysOpen, setKeysOpen] = useState(false);
  useHelpKey(() => setKeysOpen(true));
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
  const openChanges = (workspace: string, file?: string) => {
    setDrawer(false);
    setChangesOf({ workspace, file });
  };
  const openCreate = (task = "", machine = "") => {
    setDrawer(false);
    setCreate({ open: true, task, machine });
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
  useShortcut("b", flipRail);
  useShortcut("f", terminalId ? () => setFinding((n) => n + 1) : undefined);
  useShortcut("\\", terminalId ? () => (split ? setSplit(undefined) : setJump("beside")) : undefined);
  const next = upNext(machines, terminalId);
  useShortcut("j", next ? () => show(next.next.terminal) : undefined);
  const commands = useCommands({
    screen: terminalId ? "terminal" : onMachines ? "machines" : onFactory ? "factory" : "home",
    next,
    onNext: show,
    onCreate: () => openCreate(),
    onKeys: () => setKeysOpen(true),
    sidebar: narrow ? undefined : { rail, flip: flipRail },
  });

  // Never had the daemon's state: say so, rather than show an empty fleet.
  if (!state.synced && link.lostAt !== undefined) return <Unreachable link={link} />;
  const stale = link.lostAt !== undefined || remembering;
  // Each part of the fleet greys on its own, so a terminal's own screen never does.
  const frozen = { "data-fleet": true, "data-stale": stale || undefined };
  const workspaces = machines.flatMap((m) => m.workspaces);
  // A fleet with no workspaces opens on the first-run page, kept while its create is under way or
  // its workspace was made but its session failed, so what happened stays on screen.
  const firstRun = !terminalId && !onMachines && !onFactory && state.synced && (workspaces.length === 0 || launch.create.isPending || (launch.made && launch.create.isError));
  const board = !terminalId && !onMachines && !onFactory && !firstRun;

  // On a narrow screen the sidebar is a drawer over whatever is shown: the board, the machines view
  // or a terminal. Collapsed to the rail, it is only the drawer, and the rail takes its place.
  const place = rail ? "fixed inset-0 z-30 flex md:hidden" : drawer ? "fixed inset-0 z-30 flex md:static md:z-auto" : "hidden md:flex";
  const sidebar = !rail || drawer;
  return (
    // Only the daemon's own settings: a change built from the remembered fleet could overwrite a newer one.
    <FactoryContext.Provider value={sent.synced ? (sent.settings.project_factory ?? {}) : undefined}>
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
          <div className="relative flex min-h-0 flex-1">
            {rail && (
              <div {...frozen} className="hidden shrink-0 md:flex">
                <Rail machines={machines} selected={terminalId} onExpand={flipRail} onJump={openJump} onCreate={() => openCreate()} alerts={<NotifyButton {...notify} />} />
              </div>
            )}
            {sidebar && (
              <div {...frozen} className={`${place} shrink-0`}>
                <Sidebar
                  machines={machines}
                  onCreate={() => openCreate()}
                  selected={terminalId}
                  onNavigate={close}
                  onClose={close}
                  onJump={openJump}
                  onChanges={openChanges}
                  alerts={<NotifyButton {...notify} />}
                  onKeys={() => setKeysOpen(true)}
                  onRail={rail ? undefined : flipRail}
                />
              </div>
            )}
            <main className="flex min-w-0 flex-1 flex-col">
              {/* In the flow, so the terminal below gives up the room it takes; none while both are empty, and at most half the screen. */}
              {narrow && (
                <div className="flex max-h-[50dvh] shrink-0 flex-col gap-2 overflow-y-auto p-2 empty:hidden">
                  <AttentionList folded />
                  <WaitingList folded />
                </div>
              )}
              {terminalId && (
                <div {...frozen} className="flex shrink-0">
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
                        changed={changed}
                        upNext={id === terminalId ? next : undefined}
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
                        changesOf={where?.workspace.id}
                        onFile={where && ((file) => openChanges(where.workspace.id, file))}
                      />
                    </div>
                  );
                })}
                {/* Hidden while empty, so it never lies over a terminal. */}
                <div {...frozen} className="absolute inset-0 empty:hidden">
                  {firstRun && <Launchpad projects={state.settings.projects} workspaces={workspaces} launch={launch} onCreated={started} onMenu={() => setDrawer(true)} />}
                  {!terminalId && onFactory && state.synced && <Factory statuses={state.factory} settings={state.settings} machines={machines} />}
                  {/* The board, the timeline and the machines are one element, so their shared head is never remounted. */}
                  {((board && state.synced) || (!terminalId && onMachines)) && (
                    <Board
                      machines={machines}
                      projects={state.settings.projects}
                      onJump={openJump}
                      onCreated={started}
                      onChanges={openChanges}
                      traced={traced}
                      onMenu={() => setDrawer(true)}
                      history={samples}
                      connected={connected}
                      onCreate={(machine) => openCreate("", machine)}
                      onSteps={(id) => {
                        setStepsOpen(true);
                        show(id);
                      }}
                    />
                  )}
                </div>
              </div>
            </main>
            {stepsOpen && stepsOf && (
              <StepsPanel
                key={located?.session ?? terminalId}
                of={stepsOf}
                onClose={() => setStepsOpen(false)}
                stale={stale}
                onFile={located && ((file) => openChanges(located.workspace.id, file))}
              />
            )}
          </div>
          <ChangesPanel
            workspace={machines.flatMap((m) => m.workspaces).find((w) => w.id === changesOf?.workspace)}
            file={changesOf?.file}
            onClose={() => setChangesOf(undefined)}
          />
          <JumpPalette
            machines={machines}
            open={jump !== undefined}
            onOpenChange={(open) => setJump(open ? "jump" : undefined)}
            onCreate={openCreate}
            current={terminalId && located ? { terminal: terminalId, title: `${located.workspace.name} / ${located.name}` } : undefined}
            besideOnly={jump === "beside"}
            onBeside={openBeside}
            commands={commands}
          />
          <NewTask
            projects={state.settings.projects}
            workspaces={workspaces}
            open={create.open}
            onOpenChange={(open) => setCreate((c) => ({ ...c, open }))}
            task={create.task}
            machine={create.machine}
            onCreated={setLanding}
          />
          <KeySheet open={keysOpen} onOpenChange={setKeysOpen} />
          <Announcer spoken={spoken} />
          {/* One place on screen for news: what needs a person (on a wide screen), the catch-up, then the cards. */}
          <div className="pointer-events-none fixed inset-x-2 top-2 z-40 flex flex-col gap-2 md:inset-x-auto md:top-14 md:right-4 md:w-[24rem]">
            <LinkBanner link={link} remembered={remembering ? memory.at : undefined} />
            {!narrow && <AttentionList />}
            {!narrow && <WaitingList />}
            {notify.notify.away && <Away records={sent.records} synced={sent.synced} machines={machines} traced={traced} onOpen={show} onShown={setCaughtUp} />}
            {taskWarning && (
              <div
                role="alert"
                aria-label="Task warning"
                className="pointer-events-auto flex items-start gap-3 rounded-xl border border-amber-400/40 bg-neutral-900 px-4 py-3 text-sm text-amber-200 shadow-2xl shadow-black/50"
              >
                <span className="min-w-0 flex-1 break-words">{taskWarning}</span>
                <button type="button" onClick={() => setTaskWarning(undefined)} className="-my-0.5 shrink-0 rounded px-1.5 py-0.5 text-xs text-neutral-400 hover:bg-neutral-800 hover:text-neutral-100">
                  Dismiss
                </button>
              </div>
            )}
            <Toasts news={news} machines={machines} here={here} onScreen={pair ? [pair.left, pair.right] : terminalId ? [terminalId] : []} covered={notify.notify.away ? caughtUp : []} onOpen={show} />
          </div>
          {sessionIn && <NewSession key={sessionIn.id} workspace={sessionIn.id} name={sessionIn.name} onClose={() => setSessionIn(undefined)} onStarted={setLanding} />}
        </div>
      </TerminalActions>
    </FactoryContext.Provider>
  );
}

/**
 * Whether the screen is narrow, following it as it changes. A browser with no `matchMedia` counts
 * as wide.
 */
function useNarrow(): boolean {
  return !useMediaQuery(MD, true);
}

const rootRoute = createRootRoute({ component: Layout });
const indexRoute = createRoute({ getParentRoute: () => rootRoute, path: "/" });
const terminalRoute = createRoute({ getParentRoute: () => rootRoute, path: "/terminals/$terminalId" });
const machinesRoute = createRoute({ getParentRoute: () => rootRoute, path: "/machines" });
const factoryRoute = createRoute({ getParentRoute: () => rootRoute, path: "/factory" });

export const routeTree = rootRoute.addChildren([indexRoute, terminalRoute, machinesRoute, factoryRoute]);
export const router = createRouter({ routeTree });

