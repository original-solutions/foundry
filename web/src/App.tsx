import { createRootRoute, createRoute, createRouter, useNavigate, useParams } from "@tanstack/react-router";
import { useEffect, useLayoutEffect, useMemo, useState } from "react";
import { Board } from "./Board";
import { ChangesPanel } from "./ChangesPanel";
import { CreateWorkspace } from "./CreateWorkspace";
import { JumpPalette } from "./JumpPalette";
import { Sidebar } from "./Sidebar";
import { StepsPanel, type StepsOf } from "./StepsPanel";
import { TabStrip } from "./TabStrip";
import { beside, follow, type Split } from "./split";
import { loadTabs, saveTabs, touched, withoutTab, withTab } from "./tabs";
import { TerminalHeader } from "./TerminalHeader";
import { TerminalPane } from "./TerminalPane";
import { TerminalActions } from "./TerminalMenu";
import { fleet, locate, terminalRunning } from "./fleet";
import { useBriefs } from "./useBriefs";
import { useFleet } from "./useFleet";
import { AlertsButton, useAlerts } from "./AlertsBell";

/**
 * The sidebar, and the terminal the URL names (`/terminals/$terminalId`) under its header filling
 * the rest. On a narrow screen one of them has the whole width: the sidebar until a terminal is
 * open, then the terminal, with the sidebar as a drawer the header opens. Cmd+K (or Ctrl+K outside
 * the terminal, where the shell keeps it) opens the jump palette over everything.
 *
 * Every terminal opened becomes a tab above the header. The few tabs used last (and both sides of
 * a split) keep their pane attached behind the others, so going back to one is instant; an older
 * tab attaches again when shown and gets its screen and scrollback back from the holder.
 *
 * With nothing open, the board shows every workspace as a card with its live states and changes,
 * or, switched to its timeline, every session's turns on one clock; a turn opens its terminal
 * with the steps column.
 *
 * A workspace made from the new-workspace dialog (the sidebar, the board or the palette opens
 * it) is landed in: once its session shows up, its terminal opens, with the setup shell as a tab
 * beside it.
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
 * Right-clicking (or long-pressing) a terminal anywhere it is listed, or the header's More button,
 * opens its menu: open, open beside, steps, changes, and ending it. Ending a terminal that still
 * runs asks first; one that already ended is cleared at once, as are all of a workspace's ended
 * ones from its row. When the open terminal is gone, its neighbouring tab shows.
 *
 * On a wide screen a second terminal can be shown beside the open one (the header's split button,
 * Cmd+\ or Shift+Enter in the palette), each with its own header. The URL names the side with the
 * keyboard; pressing in the other side moves it there, and a terminal opened from anywhere takes
 * the side that has it. On a narrow screen only that side shows.
 */
function Layout() {
  const { state, connected } = useFleet();
  const { terminalId } = useParams({ strict: false });
  const briefs = useBriefs(state.records);
  const traced = useMemo(() => new Set(Object.keys(briefs)), [briefs]);
  const machines = useMemo(() => fleet(state.records, state.settings, briefs), [state.records, state.settings, briefs]);
  const [drawer, setDrawer] = useState(false);
  /** The palette, and whether its picks open beside the open terminal. */
  const [jump, setJump] = useState<"jump" | "beside">();
  const [split, setSplit] = useState<Split>();
  /** The new-workspace dialog: open, and the name it starts with. */
  const [create, setCreate] = useState({ open: false, name: "" });
  /** Whether the steps column is shown. */
  const [stepsOpen, setStepsOpen] = useState(false);
  /** The find bar over the terminal with the keyboard: 0 closed, and one more each time it is asked for. */
  const [finding, setFinding] = useState(0);
  /** The workspace whose changes are open. */
  const [changesOf, setChangesOf] = useState<string>();
  /** The session of a workspace just made, until it shows up and is opened. */
  const [landing, setLanding] = useState<string>();
  const navigate = useNavigate();
  const located = terminalId ? locate(machines, terminalId) : undefined;
  const stepsOf: StepsOf | undefined = !located
    ? undefined
    : located.session
      ? { kind: "session", id: located.session, name: located.name, title: located.title, glyph: located.glyph, label: located.label }
      : { kind: "shell", name: located.name };

  const gone = useMemo(() => new Set(state.records.flatMap((r) => (r.kind === "terminal" && r.deleted ? [r.body.id] : []))), [state.records]);
  const [kept, setKept] = useState(loadTabs);
  /** Tabs whose pane is attached, most recent first: the last few shown, and both sides of a split. */
  const [attached, setAttached] = useState<string[]>([]);
  /** The state each tab had when it was last shown. */
  const [seen, setSeen] = useState<Record<string, string>>({});
  // Adjusted while rendering, so a terminal opened from anywhere is a tab in the same paint. Only
  // on a change of terminal: closing the open tab must not bring it back before the URL moves on.
  const [opened, setOpened] = useState<string>();
  if (terminalId !== opened) {
    setOpened(terminalId);
    const next = follow(split, opened, terminalId);
    setSplit(next);
    if (terminalId && !gone.has(terminalId)) setKept(withTab(kept, terminalId));
    if (terminalId) setAttached(touched(attached, terminalId, next ? [next.left, next.right] : []));
  }
  if (terminalId && located && seen[terminalId] !== located.label) setSeen({ ...seen, [terminalId]: located.label });
  // The open terminal was ended or archived: show its neighbouring tab, or the board.
  const [left, setLeft] = useState<{ from: string; to?: string }>();
  if (terminalId && gone.has(terminalId) && left?.from !== terminalId) {
    setLeft({ from: terminalId, to: withoutTab(kept.filter((id) => id === terminalId || !gone.has(id)), terminalId).next });
  }
  useEffect(() => {
    if (left) void navigate(left.to ? { to: "/terminals/$terminalId", params: { terminalId: left.to } } : { to: "/" });
  }, [left, navigate]);
  if (kept.some((id) => gone.has(id))) setKept(kept.filter((id) => !gone.has(id)));
  if (attached.some((id) => id !== terminalId && gone.has(id))) setAttached(attached.filter((id) => id === terminalId || !gone.has(id)));
  if (split && (gone.has(split.left) || gone.has(split.right))) setSplit(undefined);
  useEffect(() => saveTabs(kept), [kept]);
  const pair = split && terminalId && (split.left === terminalId || split.right === terminalId) ? split : undefined;

  // Adjusted while rendering, like the tabs above: once the new session shows up, it and the
  // workspace's setup shell become tabs, and the session is the one to open.
  const [landed, setLanded] = useState<string>();
  const arrived = landing ? machines.flatMap((m) => m.workspaces).find((w) => w.sessions.some((s) => s.id === landing)) : undefined;
  if (arrived) {
    const session = arrived.sessions.find((s) => s.id === landing)!;
    setLanding(undefined);
    setKept(arrived.shells.reduce((all, shell) => withTab(all, shell.id), withTab(kept, session.terminal)));
    setLanded(session.terminal);
  }
  useEffect(() => {
    if (landed) void navigate({ to: "/terminals/$terminalId", params: { terminalId: landed } });
  }, [landed, navigate]);

  const changed = new Set(kept.filter((id) => seen[id] !== undefined && seen[id] !== locate(machines, id)?.label));
  const show = (id: string) => void navigate({ to: "/terminals/$terminalId", params: { terminalId: id } });
  const alerts = useAlerts(machines, state.synced, show);
  /** Ends the split by closing side `id`; the other side keeps or takes the keyboard. */
  const closePane = (id: string) => {
    setSplit(undefined);
    if (pair && id === terminalId) show(id === pair.left ? pair.right : pair.left);
  };
  const closeTab = (id: string) => {
    const { tabs, next } = withoutTab(kept, id);
    setKept(tabs);
    setAttached(attached.filter((t) => t !== id));
    if (pair && (id === pair.left || id === pair.right)) return closePane(id);
    if (id !== terminalId) return;
    if (next) show(next);
    else void navigate({ to: "/" });
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
  const openCreate = (name = "") => {
    setDrawer(false);
    setCreate({ open: true, name });
  };

  useLayoutEffect(() => {
    if (!drawer) return;
    const escape = (event: KeyboardEvent) => event.key === "Escape" && setDrawer(false);
    window.addEventListener("keydown", escape);
    return () => window.removeEventListener("keydown", escape);
  }, [drawer]);

  useLayoutEffect(() => {
    const shortcut = (event: KeyboardEvent) => {
      if (event.key.toLowerCase() !== "k" || event.altKey || event.shiftKey) return;
      const inTerminal = event.target instanceof Element && event.target.closest(".xterm");
      if (!event.metaKey && !(event.ctrlKey && !inTerminal)) return;
      event.preventDefault();
      event.stopPropagation();
      setDrawer(false);
      setJump((open) => (open ? undefined : "jump"));
    };
    window.addEventListener("keydown", shortcut, true);
    return () => window.removeEventListener("keydown", shortcut, true);
  }, []);

  useLayoutEffect(() => {
    if (!terminalId) return;
    const find = (event: KeyboardEvent) => {
      if (event.key.toLowerCase() !== "f" || event.altKey || event.shiftKey) return;
      const inTerminal = event.target instanceof Element && event.target.closest(".xterm");
      if (!event.metaKey && !(event.ctrlKey && !inTerminal)) return;
      event.preventDefault();
      event.stopPropagation();
      setFinding((n) => n + 1);
    };
    window.addEventListener("keydown", find, true);
    return () => window.removeEventListener("keydown", find, true);
  }, [terminalId]);

  useLayoutEffect(() => {
    if (!terminalId) return;
    const toggle = (event: KeyboardEvent) => {
      if (event.key !== "\\" || event.altKey || event.shiftKey) return;
      const inTerminal = event.target instanceof Element && event.target.closest(".xterm");
      if (!event.metaKey && !(event.ctrlKey && !inTerminal)) return;
      event.preventDefault();
      event.stopPropagation();
      if (split) setSplit(undefined);
      else setJump("beside");
    };
    window.addEventListener("keydown", toggle, true);
    return () => window.removeEventListener("keydown", toggle, true);
  }, [terminalId, split]);

  const place = !terminalId ? "flex w-full md:w-auto" : drawer ? "fixed inset-0 z-30 flex md:static md:z-auto" : "hidden md:flex";
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
    >
      <div className="flex h-dvh overflow-hidden bg-neutral-950 text-neutral-100">
        <div className={`${place} shrink-0`}>
          <Sidebar
            machines={machines}
            onCreate={() => openCreate()}
            selected={terminalId}
            connected={connected}
            onNavigate={close}
            onClose={terminalId ? close : undefined}
            onJump={openJump}
            onChanges={openChanges}
            alerts={<AlertsButton {...alerts} />}
          />
        </div>
        <main className={`${terminalId ? "flex" : "hidden md:flex"} min-w-0 flex-1 flex-col`}>
          <TabStrip
            machines={machines}
            tabs={kept}
            active={terminalId}
            beside={pair && (pair.left === terminalId ? pair.right : pair.left)}
            changed={changed}
            onClose={closeTab}
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
                  />
                </div>
              );
            })}
            {!terminalId && (
              <Board
                machines={machines}
                onJump={openJump}
                onCreate={() => openCreate()}
                onChanges={openChanges}
                traced={traced}
                onSteps={(id) => {
                  setStepsOpen(true);
                  show(id);
                }}
              />
            )}
          </div>
        </main>
        {stepsOpen && stepsOf && <StepsPanel key={located?.session ?? terminalId} of={stepsOf} onClose={() => setStepsOpen(false)} />}
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
        <CreateWorkspace
          projects={state.settings.projects}
          workspaces={machines.flatMap((m) => m.workspaces)}
          open={create.open}
          onOpenChange={(open) => setCreate((c) => ({ ...c, open }))}
          name={create.name}
          onCreated={setLanding}
        />
      </div>
    </TerminalActions>
  );
}

const rootRoute = createRootRoute({ component: Layout });
const indexRoute = createRoute({ getParentRoute: () => rootRoute, path: "/" });
const terminalRoute = createRoute({ getParentRoute: () => rootRoute, path: "/terminals/$terminalId" });

export const routeTree = rootRoute.addChildren([indexRoute, terminalRoute]);
export const router = createRouter({ routeTree });

