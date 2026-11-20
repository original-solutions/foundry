import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRouter, RouterProvider } from "@tanstack/react-router";
import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { UiMessage } from "./api";
import { router, routeTree } from "./App";
import { ME, sample, session, settings, terminal, workspace } from "./test-fixtures";
import { FakeSocket } from "./test-utils";

// jsdom cannot run xterm.js.
vi.mock("@xterm/xterm", () => ({
  Terminal: class {
    cols = 80;
    rows = 24;
    loadAddon() {}
    open() {}
    reset() {}
    write() {}
    onData() {
      return { dispose() {} };
    }
    onResize() {
      return { dispose() {} };
    }
    dispose() {}
  },
}));
vi.mock("@xterm/addon-fit", () => ({ FitAddon: class { fit() {} } }));

const sockets = (path: string) => FakeSocket.all.filter((s) => s.url.endsWith(path));

beforeEach(() => {
  FakeSocket.install();
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
});

afterEach(() => vi.unstubAllGlobals());

describe("App", () => {
  it("gives the pane its terminal's state from the events socket, so an exited one is not tried again", async () => {
    window.history.pushState({}, "", "/terminals/t1");
    await router.load();
    render(
      <QueryClientProvider client={new QueryClient()}>
        <RouterProvider router={router} />
      </QueryClientProvider>,
    );

    const events = sockets("/api/events")[0];
    const state: UiMessage = {
      type: "state",
      protocol: 1,
      records: [sample(ME, 1), workspace(ME, "w1", "fix login"), terminal(ME, "t1", "w1", "exited", { exit_code: 0 })],
      settings: settings(),
    };
    act(() => events.open());
    act(() => events.deliver(JSON.stringify(state)));
    const pane = sockets("/api/terminals/t1")[0];
    act(() => pane.end(1006));
    await new Promise((resolve) => setTimeout(resolve, 700));

    expect(sockets("/api/terminals/t1")).toHaveLength(1);
    expect(screen.getByText(/no longer running/i)).toBeDefined();
  });

  async function open(path: string, records: UiMessage & { type: "state" }) {
    const fresh = createRouter({ routeTree, history: createMemoryHistory({ initialEntries: [path] }) });
    await fresh.load();
    render(
      <QueryClientProvider client={new QueryClient()}>
        <RouterProvider router={fresh} />
      </QueryClientProvider>,
    );
    const events = sockets("/api/events").at(-1)!;
    act(() => events.open());
    act(() => events.deliver(JSON.stringify(records)));
  }

  const live: UiMessage & { type: "state" } = {
    type: "state",
    protocol: 1,
    records: [
      sample(ME, 1),
      workspace(ME, "w1", "fix login"),
      terminal(ME, "t1", "w1", "running"),
      session(ME, "s1", "w1", "t1", "working", { model: "sonnet" }),
      terminal(ME, "t2", "w1", "running"),
    ],
    settings: settings(),
  };

  it("heads the open terminal with its workspace, its name, its machine and its state", async () => {
    await open("/terminals/t1", live);

    const header = screen.getByLabelText("Open terminal");
    expect(within(header).getByText("fix login")).toBeDefined();
    expect(within(header).getByText("sonnet")).toBeDefined();
    expect(within(header).getByText("studio")).toBeDefined();
    expect(within(header).getByText("Working")).toBeDefined();
  });

  it("opens the sidebar as a drawer from the header, and closes it on a pick or on Escape", async () => {
    await open("/terminals/t1", live);
    const menu = screen.getByRole("button", { name: "Workspaces" });
    expect(menu.getAttribute("aria-expanded")).toBe("false");

    fireEvent.click(menu);
    expect(menu.getAttribute("aria-expanded")).toBe("true");
    fireEvent.click(screen.getByRole("link", { name: /^shell/ }));
    await screen.findByText("shell", { selector: "header *" });
    expect(screen.getByRole("button", { name: "Workspaces" }).getAttribute("aria-expanded")).toBe("false");

    fireEvent.click(screen.getByRole("button", { name: "Workspaces" }));
    fireEvent.keyDown(window, { key: "Escape" });
    expect(screen.getByRole("button", { name: "Workspaces" }).getAttribute("aria-expanded")).toBe("false");

    fireEvent.click(screen.getByRole("button", { name: "Workspaces" }));
    fireEvent.click(screen.getByRole("button", { name: "Close workspaces" }));
    expect(screen.getByRole("button", { name: "Workspaces" }).getAttribute("aria-expanded")).toBe("false");
  });
});
