import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, RouterProvider } from "@tanstack/react-router";
import { render } from "@testing-library/react";
import type { ReactNode } from "react";

/** Renders `ui` inside a query client and a router that knows the terminal route, so links resolve. */
export async function renderApp(ui: ReactNode) {
  const root = createRootRoute({ component: () => <>{ui}</> });
  const terminal = createRoute({ getParentRoute: () => root, path: "/terminals/$terminalId" });
  const router = createRouter({
    routeTree: root.addChildren([terminal]),
    history: createMemoryHistory({ initialEntries: ["/"] }),
  });
  await router.load();
  const client = new QueryClient({ defaultOptions: { mutations: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
}
