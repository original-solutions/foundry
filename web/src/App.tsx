import { createRootRoute, createRoute, createRouter, useNavigate, useParams } from "@tanstack/react-router";
import { useEffect, useLayoutEffect, useMemo, useState } from "react";
import { CreateWorkspace } from "./CreateWorkspace";
import { JumpPalette, jumpKey, SearchIcon } from "./JumpPalette";
import { Sidebar } from "./Sidebar";
import { TabStrip } from "./TabStrip";
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
 */
function Layout() {
  const { state, connected } = useFleet();
  const { terminalId } = useParams({ strict: false });
  const machines = useMemo(() => fleet(state.records, state.settings), [state.records, state.settings]);
  const [drawer, setDrawer] = useState(false);
  const [jump, setJump] = useState(false);
  /** The new-workspace dialog: open, and the name it starts with. */
  const [create, setCreate] = useState({ open: false, name: "" });
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
    if (terminalId && !gone.has(terminalId)) setKept(withTab(kept, terminalId));
    if (terminalId) setAttached(withTab(attached, terminalId));
  }
  if (terminalId && located && seen[terminalId] !== located.label) setSeen({ ...seen, [terminalId]: located.label });
  if (kept.some((id) => gone.has(id))) setKept(kept.filter((id) => !gone.has(id)));
  if (attached.some((id) => id !== terminalId && gone.has(id))) setAttached(attached.filter((id) => id === terminalId || !gone.has(id)));
  useEffect(() => saveTabs(kept), [kept]);

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
  const closeTab = (id: string) => {
    const { tabs, next } = withoutTab(kept, id);
    setKept(tabs);
    setAttached(attached.filter((t) => t !== id));
    if (id !== terminalId) return;
    if (next) void navigate({ to: "/terminals/$terminalId", params: { terminalId: next } });
    else void navigate({ to: "/" });
  };
  const close = () => setDrawer(false);
  const openJump = () => {
    setDrawer(false);
    setJump(true);
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
      setJump((open) => !open);
    };
    window.addEventListener("keydown", shortcut, true);
    return () => window.removeEventListener("keydown", shortcut, true);
  }, []);

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
        />
      </div>
      <main className={`${terminalId ? "flex" : "hidden md:flex"} min-w-0 flex-1 flex-col`}>
        <TabStrip machines={machines} tabs={kept} active={terminalId} changed={changed} onClose={closeTab} />
        {terminalId && <TerminalHeader located={located} drawerOpen={drawer} onMenu={() => setDrawer(true)} onJump={openJump} />}
        <div className="relative min-h-0 flex-1">
          {attached.map((id) => (
            <div key={id} className={`absolute inset-0 ${id === terminalId ? "" : "invisible"}`}>
              <TerminalPane id={id} running={terminalRunning(state, id)} active={id === terminalId} />
            </div>
          ))}
          {!terminalId && <Home machines={machines} onJump={openJump} onCreate={() => openCreate()} />}
        </div>
      </main>
      <JumpPalette machines={machines} open={jump} onOpenChange={setJump} onCreate={openCreate} />
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
