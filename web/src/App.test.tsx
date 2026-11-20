import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRouter, RouterProvider } from "@tanstack/react-router";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { UiMessage } from "./api";
import { router, routeTree } from "./App";
import { ME, sample, session, settings, terminal, workspace } from "./test-fixtures";
import { FakeSocket, json, mockFetch } from "./test-utils";

// jsdom cannot run xterm.js.
vi.mock("@xterm/xterm", () => ({
  Terminal: class {
    cols = 80;
    rows = 24;
    loadAddon() {}
    open() {}
    focus() {}
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
    return fresh;
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

  it("opens the jump palette on Cmd+K, filters as you type and opens the pick with Enter", async () => {
    const router = await open("/", live);
    expect(screen.queryByRole("dialog")).toBeNull();

    fireEvent.keyDown(window, { key: "k", metaKey: true });
    const dialog = await screen.findByRole("dialog", { name: "Jump to" });
    const input = within(dialog).getByRole("combobox");
    expect(within(dialog).getAllByRole("option")).toHaveLength(3);

    fireEvent.change(input, { target: { value: "login shell" } });
    expect(within(dialog).getAllByRole("option").map((o) => o.textContent)).toEqual([expect.stringContaining("fix login / shell"), expect.stringContaining("New shell in fix login")]);
    expect(within(dialog).getAllByRole("option")[0].getAttribute("aria-selected")).toBe("true");
    fireEvent.keyDown(input, { key: "Enter" });

    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t2"));
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.getByText("shell", { selector: "header *" })).toBeDefined();
  });

  it("moves the pick with the arrow keys and says when nothing matches", async () => {
    const router = await open("/", live);
    fireEvent.click(screen.getByRole("button", { name: /jump to a session or shell/i }));
    const dialog = await screen.findByRole("dialog", { name: "Jump to" });
    const input = within(dialog).getByRole("combobox");

    const options = within(dialog).getAllByRole("option");
    fireEvent.keyDown(input, { key: "ArrowUp" });
    expect(input.getAttribute("aria-activedescendant")).toBe(options[2].id);
    fireEvent.keyDown(input, { key: "ArrowDown" });
    fireEvent.keyDown(input, { key: "ArrowDown" });
    expect(input.getAttribute("aria-activedescendant")).toBe(within(dialog).getAllByRole("option")[1].id);
    fireEvent.keyDown(input, { key: "Enter" });
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t2"));

    fireEvent.keyDown(window, { key: "k", ctrlKey: true });
    const again = await screen.findByRole("dialog", { name: "Jump to" });
    fireEvent.change(within(again).getByRole("combobox"), { target: { value: "zzz" } });
    expect(within(again).getByText(/nothing matches/i)).toBeDefined();
  });

  it("leaves Ctrl+K to the shell when the terminal has focus", async () => {
    await open("/terminals/t1", live);
    const inside = document.createElement("textarea");
    const xterm = document.createElement("div");
    xterm.className = "xterm";
    xterm.append(inside);
    document.body.append(xterm);

    fireEvent.keyDown(inside, { key: "k", ctrlKey: true });
    expect(screen.queryByRole("dialog")).toBeNull();
    fireEvent.keyDown(inside, { key: "k", metaKey: true });
    expect(await screen.findByRole("dialog", { name: "Jump to" })).toBeDefined();
    xterm.remove();
  });

  it("starts a new shell from the palette and opens it", async () => {
    const calls = mockFetch(json(201, { id: "t9" }));
    const router = await open("/", live);
    fireEvent.keyDown(window, { key: "k", metaKey: true });
    const dialog = await screen.findByRole("dialog", { name: "Jump to" });
    fireEvent.change(within(dialog).getByRole("combobox"), { target: { value: "new shell" } });
    fireEvent.click(within(dialog).getByRole("option", { name: /new shell in fix login/i }));

    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t9"));
    expect(calls).toEqual([{ url: "/api/workspaces/w1/shells", method: "POST", body: undefined }]);
    expect(screen.queryByRole("dialog")).toBeNull();
  });
});
