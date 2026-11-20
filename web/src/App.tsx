import { createRootRoute, createRoute, createRouter, useParams } from "@tanstack/react-router";
import { useLayoutEffect, useMemo, useState } from "react";
import { Sidebar } from "./Sidebar";
import { TerminalHeader } from "./TerminalHeader";
import { TerminalPane } from "./TerminalPane";
import { fleet, locate, terminalRunning } from "./fleet";
import { useFleet } from "./useFleet";

/**
 * The sidebar, and the terminal the URL names (`/terminals/$terminalId`) under its header filling
 * the rest. On a narrow screen one of them has the whole width: the sidebar until a terminal is
 * open, then the terminal, with the sidebar as a drawer the header opens.
 */
function Layout() {
  const { state, connected } = useFleet();
  const { terminalId } = useParams({ strict: false });
  const machines = useMemo(() => fleet(state.records, state.settings), [state.records, state.settings]);
  const [drawer, setDrawer] = useState(false);
  const close = () => setDrawer(false);

  useLayoutEffect(() => {
    if (!drawer) return;
    const escape = (event: KeyboardEvent) => event.key === "Escape" && setDrawer(false);
    window.addEventListener("keydown", escape);
    return () => window.removeEventListener("keydown", escape);
  }, [drawer]);

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
        />
      </div>
      <main className={`${terminalId ? "flex" : "hidden md:flex"} min-w-0 flex-1 flex-col`}>
        {terminalId ? (
          <>
            <TerminalHeader located={locate(machines, terminalId)} drawerOpen={drawer} onMenu={() => setDrawer(true)} />
            <div className="min-h-0 flex-1">
              <TerminalPane id={terminalId} running={terminalRunning(state, terminalId)} />
            </div>
          </>
        ) : (
          <p className="p-4 text-neutral-400">Pick a session or shell on the left.</p>
        )}
      </main>
    </div>
  );
}

const rootRoute = createRootRoute({ component: Layout });
const indexRoute = createRoute({ getParentRoute: () => rootRoute, path: "/" });
const terminalRoute = createRoute({ getParentRoute: () => rootRoute, path: "/terminals/$terminalId" });

export const routeTree = rootRoute.addChildren([indexRoute, terminalRoute]);
export const router = createRouter({ routeTree });
