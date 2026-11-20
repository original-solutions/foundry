import { createRootRoute, createRoute, createRouter, useParams } from "@tanstack/react-router";
import { useLayoutEffect, useMemo, useState } from "react";
import { JumpPalette, jumpKey, SearchIcon } from "./JumpPalette";
import { Sidebar } from "./Sidebar";
import { TerminalHeader } from "./TerminalHeader";
import { TerminalPane } from "./TerminalPane";
import { fleet, locate, terminalRunning, type MachineView } from "./fleet";
import { useFleet } from "./useFleet";

/**
 * The sidebar, and the terminal the URL names (`/terminals/$terminalId`) under its header filling
 * the rest. On a narrow screen one of them has the whole width: the sidebar until a terminal is
 * open, then the terminal, with the sidebar as a drawer the header opens. Cmd+K (or Ctrl+K outside
 * the terminal, where the shell keeps it) opens the jump palette over everything.
 */
function Layout() {
  const { state, connected } = useFleet();
  const { terminalId } = useParams({ strict: false });
  const machines = useMemo(() => fleet(state.records, state.settings), [state.records, state.settings]);
  const [drawer, setDrawer] = useState(false);
  const [jump, setJump] = useState(false);
  const close = () => setDrawer(false);
  const openJump = () => {
    setDrawer(false);
    setJump(true);
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
          projects={state.settings.projects}
          selected={terminalId}
          connected={connected}
          onNavigate={close}
          onClose={terminalId ? close : undefined}
          onJump={openJump}
        />
      </div>
      <main className={`${terminalId ? "flex" : "hidden md:flex"} min-w-0 flex-1 flex-col`}>
        {terminalId ? (
          <>
            <TerminalHeader located={locate(machines, terminalId)} drawerOpen={drawer} onMenu={() => setDrawer(true)} onJump={openJump} />
            <div className="min-h-0 flex-1">
              <TerminalPane id={terminalId} running={terminalRunning(state, terminalId)} />
            </div>
          </>
        ) : (
          <Home machines={machines} onJump={openJump} />
        )}
      </main>
      <JumpPalette machines={machines} open={jump} onOpenChange={setJump} />
    </div>
  );
}

/** The desktop view with nothing open: what the fleet holds, and the way to jump into it. */
function Home({ machines, onJump }: { machines: MachineView[]; onJump: () => void }) {
  const workspaces = machines.flatMap((m) => m.workspaces);
  const sessions = workspaces.reduce((n, w) => n + w.sessions.length, 0);
  const shells = workspaces.reduce((n, w) => n + w.shells.length, 0);
  const count = (n: number, one: string) => `${n} ${one}${n === 1 ? "" : "s"}`;
  return (
    <div className="flex flex-1 flex-col items-center justify-center gap-5 px-6 pb-[12vh]">
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
      <p className="text-xs text-neutral-600">Type a workspace, model, state or machine. Or pick one on the left.</p>
    </div>
  );
}

const rootRoute = createRootRoute({ component: Layout });
const indexRoute = createRoute({ getParentRoute: () => rootRoute, path: "/" });
const terminalRoute = createRoute({ getParentRoute: () => rootRoute, path: "/terminals/$terminalId" });

export const routeTree = rootRoute.addChildren([indexRoute, terminalRoute]);
export const router = createRouter({ routeTree });
