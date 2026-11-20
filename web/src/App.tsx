import { createRootRoute, createRoute, createRouter, useParams } from "@tanstack/react-router";
import { useMemo } from "react";
import { Sidebar } from "./Sidebar";
import { TerminalPane } from "./TerminalPane";
import { fleet, terminalRunning } from "./fleet";
import { useFleet } from "./useFleet";

/** The sidebar, and the terminal the URL names (`/terminals/$terminalId`) filling the rest. */
function Layout() {
  const { state, connected } = useFleet();
  const { terminalId } = useParams({ strict: false });
  const machines = useMemo(() => fleet(state.records, state.settings), [state.records, state.settings]);
  return (
    <div className="flex h-screen overflow-hidden bg-neutral-950 text-neutral-100">
      <Sidebar machines={machines} projects={state.settings.projects} selected={terminalId} connected={connected} />
      <main className="min-w-0 flex-1">
        {terminalId ? (
          <TerminalPane id={terminalId} running={terminalRunning(state, terminalId)} />
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

export const router = createRouter({ routeTree: rootRoute.addChildren([indexRoute, terminalRoute]) });
