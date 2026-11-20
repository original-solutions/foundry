import { createRootRoute, createRoute, createRouter, Outlet, useParams } from "@tanstack/react-router";
import { Sidebar } from "./Sidebar";
import { TerminalPane } from "./TerminalPane";
import { fleet } from "./fleet";
import { useFleet } from "./useFleet";

function Layout() {
  const { state, connected } = useFleet();
  const { terminalId } = useParams({ strict: false });
  return (
    <div className="flex h-screen bg-neutral-950 text-neutral-100">
      <Sidebar machines={fleet(state.records, state.settings)} projects={state.settings.projects} selected={terminalId} connected={connected} />
      <main className="min-w-0 flex-1">
        <Outlet />
      </main>
    </div>
  );
}

const rootRoute = createRootRoute({ component: Layout });
const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  component: () => <p className="p-4 text-neutral-400">Pick a session or shell on the left.</p>,
});
const terminalRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/terminals/$terminalId",
  component: function TerminalRoute() {
    return <TerminalPane id={terminalRoute.useParams().terminalId} />;
  },
});

export const router = createRouter({ routeTree: rootRoute.addChildren([indexRoute, terminalRoute]) });
