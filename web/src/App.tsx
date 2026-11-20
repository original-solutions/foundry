import { createRootRoute, createRoute, createRouter, useNavigate, useParams } from "@tanstack/react-router";
import { useEffect, useLayoutEffect, useMemo, useState } from "react";
import { ChangesPanel } from "./ChangesPanel";
import { CreateWorkspace } from "./CreateWorkspace";
import { JumpPalette, jumpKey, SearchIcon } from "./JumpPalette";
import { Sidebar } from "./Sidebar";
import { TabStrip } from "./TabStrip";
import { beside, follow, type Split } from "./split";
import { loadTabs, saveTabs, withoutTab, withTab } from "./tabs";
import { TerminalHeader } from "./TerminalHeader";
import { TerminalPane } from "./TerminalPane";
import { fleet, locate, terminalRunning, type MachineView } from "./fleet";
import { useFleet } from "./useFleet";

/**
 * The sidebar, and the terminal the URL names (`/terminals/$terminalId`) under its header filling
 * the rest. On a narrow screen one of them has the whole width: the sidebar until a terminal is
 * open, then the terminal, with the sidebar as a drawer the header opens. Cmd+K (or Ctrl+K outside
 * the terminal, where the shell keeps it) opens the jump palette over everything.
 *
 * Every terminal opened becomes a tab above the header. A tab shown since the page loaded keeps
 * its pane attached behind the others, so going back to it is instant.
 *
 * A workspace made from the new-workspace dialog (the sidebar, the fleet view or the palette opens
 * it) is landed in: once its session shows up, its terminal opens, with the setup shell as a tab
 * beside it.
 *
 * A workspace's changes since its base open as a sheet from the header or the workspace's row.
 *
 * On a wide screen a second terminal can be shown beside the open one (the header's split button,
 * Cmd+\ or Shift+Enter in the palette), each with its own header. The URL names the side with the
 * keyboard; pressing in the other side moves it there, and a terminal opened from anywhere takes
 * the side that has it. On a narrow screen only that side shows.
 */
function Layout() {
  const { state, connected } = useFleet();
  const { terminalId } = useParams({ strict: false });
  const machines = useMemo(() => fleet(state.records, state.settings), [state.records, state.settings]);
  const [drawer, setDrawer] = useState(false);
  /** The palette, and whether its picks open beside the open terminal. */
  const [jump, setJump] = useState<"jump" | "beside">();
  const [split, setSplit] = useState<Split>();
  /** The new-workspace dialog: open, and the name it starts with. */
  const [create, setCreate] = useState({ open: false, name: "" });
  /** The workspace whose changes are open. */
  const [changesOf, setChangesOf] = useState<string>();
  /** The session of a workspace just made, until it shows up and is opened. */
  const [landing, setLanding] = useState<string>();
  const navigate = useNavigate();
  const located = terminalId ? locate(machines, terminalId) : undefined;

  const gone = useMemo(() => new Set(state.records.flatMap((r) => (r.kind === "terminal" && r.deleted ? [r.body.id] : []))), [state.records]);
  const [kept, setKept] = useState(loadTabs);
  /** Tabs whose pane is attached: the ones shown since the page loaded. */
  const [attached, setAttached] = useState<string[]>([]);
  /** The state each tab had when it was last shown. */
  const [seen, setSeen] = useState<Record<string, string>>({});
  // Adjusted while rendering, so a terminal opened from anywhere is a tab in the same paint. Only
  // on a change of terminal: closing the open tab must not bring it back before the URL moves on.
  const [opened, setOpened] = useState<string>();
  if (terminalId !== opened) {
    setOpened(terminalId);
    setSplit(follow(split, opened, terminalId));
    if (terminalId && !gone.has(terminalId)) setKept(withTab(kept, terminalId));
    if (terminalId) setAttached(withTab(attached, terminalId));
  }
  if (terminalId && located && seen[terminalId] !== located.label) setSeen({ ...seen, [terminalId]: located.label });
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
                  located={id === terminalId ? located : locate(machines, id)}
                  drawerOpen={drawer}
                  onMenu={() => setDrawer(true)}
                  onJump={openJump}
                  onChanges={openChanges}
                  onSplit={pair ? undefined : () => setJump("beside")}
                  onClosePane={pair ? () => closePane(id) : undefined}
                  beside={id !== terminalId}
                />
              </div>
            ))}
          </div>
        )}
        <div className="relative min-h-0 flex-1">
          {attached.map((id) => {
            const side = pair && (id === pair.left ? "left" : id === pair.right ? "right" : undefined);
            const place = side === "left" ? "inset-y-0 left-0 right-0 md:right-1/2" : side === "right" ? "inset-y-0 right-0 left-0 md:left-1/2 md:border-l md:border-neutral-800" : "inset-0";
            const seen = id === terminalId ? "" : side ? "invisible md:visible" : "invisible";
            return (
              <div key={id} data-testid={`pane-${id}`} onPointerDownCapture={side && focusPane(id)} onFocusCapture={side && focusPane(id)} className={`absolute ${place} ${seen}`}>
                <TerminalPane id={id} running={terminalRunning(state, id)} active={id === terminalId} />
              </div>
            );
          })}
          {!terminalId && <Home machines={machines} onJump={openJump} onCreate={() => openCreate()} />}
        </div>
      </main>
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
  );
}

/** The desktop view with nothing open: what the fleet holds, and the way to jump into it. */
function Home({ machines, onJump, onCreate }: { machines: MachineView[]; onJump: () => void; onCreate: () => void }) {
  const workspaces = machines.flatMap((m) => m.workspaces);
  const sessions = workspaces.reduce((n, w) => n + w.sessions.length, 0);
  const shells = workspaces.reduce((n, w) => n + w.shells.length, 0);
  const count = (n: number, one: string) => `${n} ${one}${n === 1 ? "" : "s"}`;
  return (
    <div className="absolute inset-0 flex flex-col items-center justify-center gap-5 px-6 pb-[12vh]">
      <p className="text-sm text-neutral-500">
        {count(sessions, "session")} · {count(shells, "shell")} · {count(workspaces.length, "workspace")} · {count(machines.length, "machine")}
      </p>
      <button
        type="button"
        onClick={onJump}
        className="group flex w-full max-w-xl items-center gap-3 rounded-xl border border-neutral-700/70 bg-neutral-900 px-4 py-3.5 text-left text-base text-neutral-400 shadow-lg shadow-black/40 transition-colors hover:border-neutral-500 hover:text-neutral-200"
      >
        <SearchIcon className="size-5 shrink-0 text-neutral-500 group-hover:text-neutral-300" />
        <span className="flex-1">Jump to a session or shell…</span>
        <kbd className="rounded border border-neutral-700 px-1.5 py-0.5 text-xs text-neutral-400">{jumpKey}</kbd>
      </button>
      <p className="text-xs text-neutral-600">
        Type a workspace, model, state or machine. Or{" "}
        <button type="button" onClick={onCreate} className="text-neutral-400 underline decoration-neutral-700 underline-offset-4 hover:text-neutral-200">
          start a new workspace
        </button>
        .
      </p>
    </div>
  );
}

const rootRoute = createRootRoute({ component: Layout });
const indexRoute = createRoute({ getParentRoute: () => rootRoute, path: "/" });
const terminalRoute = createRoute({ getParentRoute: () => rootRoute, path: "/terminals/$terminalId" });

export const routeTree = rootRoute.addChildren([indexRoute, terminalRoute]);
export const router = createRouter({ routeTree });
