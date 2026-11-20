import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRouter, RouterProvider } from "@tanstack/react-router";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Briefs, UiMessage } from "./api";
import { router, routeTree } from "./App";
import { ME, sample, session, settings, terminal, workspace } from "./test-fixtures";
import { FakeSocket, json, mockFetch, type Call } from "./test-utils";

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
    onWriteParsed() {
      return { dispose() {} };
    }
    onScroll() {
      return { dispose() {} };
    }
    buffer = { active: { length: 1, viewportY: 0, baseY: 0, cursorY: 0, getLine: () => ({ isWrapped: false, translateToString: () => "cargo test: 1 error" }) } };
    registerMarker() {
      return { dispose() {} };
    }
    registerDecoration() {
      return { dispose() {} };
    }
    scrollToLine() {}
    dispose() {}
  },
}));
vi.mock("@xterm/addon-fit", () => ({ FitAddon: class { fit() {} } }));

/**
 * Like `mockFetch`, but the board's reads of each workspace's changes get an empty answer, and the
 * reads of session briefs get `briefs`; both are left out of the calls, so a test sees only the
 * requests it makes.
 */
let briefs: Briefs = {};

function mockActions(...answers: Response[]): Call[] {
  const calls: Call[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init: RequestInit = {}) => {
      if (url.endsWith("/changes")) return json(200, { base: "main", commits: [], files: [] });
      if (url.endsWith("/api/briefs")) return json(200, briefs);
      calls.push({ url, method: init.method ?? "GET", body: init.body ? JSON.parse(String(init.body)) : undefined });
      const next = answers.shift();
      if (!next) throw new Error("unexpected request");
      return next;
    }),
  );
  return calls;
}

const sockets = (path: string) => FakeSocket.all.filter((s) => s.url.endsWith(path));

beforeEach(() => {
  briefs = {};
  localStorage.clear();
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

  it("names the open session by its first prompt in the header, the tab and the sidebar, and says what it is doing", async () => {
    briefs = { s1: { title: "fix the login bug", now: { kind: "tool", text: "Edit src/login.ts" } } };
    mockActions();
    await open("/terminals/t1", live);

    const header = screen.getByLabelText("Open terminal");
    expect(await within(header).findByText("· fix the login bug")).toBeDefined();
    expect(screen.getByRole("tab", { name: /fix login \/ fix the login bug/ })).toBeDefined();
    const sidebar = screen.getByRole("group", { name: "fix login" });
    expect(within(sidebar).getByText("Edit src/login.ts")).toBeDefined();
  });

  it("titles the tab with the fleet's state, and a desktop alert's click opens the session it names", async () => {
    const shown: { title: string; onclick: (() => void) | null; close: () => void }[] = [];
    vi.stubGlobal(
      "Notification",
      class {
        static permission = "granted";
        onclick: (() => void) | null = null;
        close() {}
        constructor(readonly title: string) {
          shown.push(this);
        }
      },
    );
    vi.spyOn(window, "focus").mockImplementation(() => {});
    localStorage.setItem("foundry.alerts", "on");
    mockActions();
    const fresh = await open("/", live);
    expect(document.title).toBe("Foundry · 1 working");
    expect(screen.getByRole("button", { name: "Alerts on" })).toBeDefined();

    vi.spyOn(document, "hasFocus").mockReturnValue(false);
    const events = sockets("/api/events").at(-1)!;
    act(() => events.deliver(JSON.stringify({ type: "record", protocol: 1, record: session(ME, "s1", "w1", "t1", "idle", { model: "sonnet", version: 2 }) })));
    expect(document.title).toBe("Foundry");
    expect(shown.map((n) => n.title)).toEqual(["fix login"]);
    act(() => shown[0].onclick?.());
    await waitFor(() => expect(fresh.state.location.pathname).toBe("/terminals/t1"));
    vi.restoreAllMocks();
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
    expect(within(dialog).getAllByRole("option").map((o) => o.textContent)).toEqual([
      expect.stringContaining("fix login / shell"),
      expect.stringContaining("New shell in fix login"),
      expect.stringContaining("New workspace “login shell”"),
    ]);
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

  it("shows the board with nothing open, and a card's changes open the changes sheet", async () => {
    mockActions();
    await open("/", live);
    const card = screen.getByRole("article", { name: "fix login" });
    expect(within(screen.getByRole("region", { name: "Working" })).getByRole("article", { name: "fix login" })).toBe(card);
    await waitFor(() => expect(within(card).getByText("No changes yet")).toBeDefined());
    fireEvent.click(within(card).getByRole("button", { name: "Changes in fix login" }));
    expect(await screen.findByRole("dialog", { name: /changes in fix login/i })).toBeDefined();
  });

  it("starts a new shell from the palette and opens it", async () => {
    const calls = mockActions(json(201, { id: "t9" }));
    const router = await open("/", live);
    fireEvent.keyDown(window, { key: "k", metaKey: true });
    const dialog = await screen.findByRole("dialog", { name: "Jump to" });
    fireEvent.change(within(dialog).getByRole("combobox"), { target: { value: "new shell" } });
    fireEvent.click(within(dialog).getByRole("option", { name: /new shell in fix login/i }));

    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t9"));
    expect(calls).toEqual([{ url: "/api/workspaces/w1/shells", method: "POST", body: undefined }]);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("lands in the new workspace's session once it starts, with its setup shell as a tab beside it", async () => {
    const calls = mockActions(json(201, { id: "w9" }), json(201, { id: "s9" }));
    const router = await open("/", live);

    fireEvent.click(screen.getByRole("button", { name: "New workspace" }));
    const dialog = await screen.findByRole("dialog", { name: "New workspace" });
    fireEvent.change(within(dialog).getByLabelText("Name"), { target: { value: "add search" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Create and open" }));
    await waitFor(() => expect(calls).toHaveLength(2));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(router.state.location.pathname).toBe("/");

    const events = sockets("/api/events").at(-1)!;
    const arrive = (record: ReturnType<typeof workspace>) => act(() => events.deliver(JSON.stringify({ type: "record", protocol: 1, record })));
    arrive(workspace(ME, "w9", "add search"));
    arrive(terminal(ME, "t8", "w9", "running"));
    arrive(terminal(ME, "t9", "w9", "running"));
    arrive(session(ME, "s9", "w9", "t9", "starting"));

    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t9"));
    expect(screen.getAllByRole("tab").map((t) => t.textContent)).toEqual([expect.stringContaining("add search / opus"), expect.stringContaining("add search / shell")]);
    expect(screen.getAllByRole("tab")[0].getAttribute("aria-selected")).toBe("true");
  });

  it("offers a new workspace named from what you typed in the palette", async () => {
    await open("/", live);
    fireEvent.keyDown(window, { key: "k", metaKey: true });
    const palette = await screen.findByRole("dialog", { name: "Jump to" });
    const input = within(palette).getByRole("combobox");
    fireEvent.change(input, { target: { value: "zzz nothing" } });

    expect(within(palette).getAllByRole("option").map((o) => o.textContent)).toEqual([expect.stringContaining("New workspace “zzz nothing”")]);
    fireEvent.keyDown(input, { key: "Enter" });

    const dialog = await screen.findByRole("dialog", { name: "New workspace" });
    expect((within(dialog).getByLabelText("Name") as HTMLInputElement).value).toBe("zzz nothing");
    expect(screen.queryByRole("dialog", { name: "Jump to" })).toBeNull();
  });

  it("keeps every terminal you open as a tab with its state, and a tab you go back to has not reconnected", async () => {
    const router = await open("/terminals/t1", live);
    fireEvent.click(screen.getByRole("link", { name: /^shell/ }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t2"));

    const strip = screen.getByRole("tablist", { name: "Open terminals" });
    const tabs = within(strip).getAllByRole("tab");
    expect(tabs.map((t) => t.textContent)).toEqual([expect.stringContaining("fix login / sonnet"), expect.stringContaining("fix login / shell")]);
    expect(tabs[1].getAttribute("aria-selected")).toBe("true");

    fireEvent.click(tabs[0]);
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t1"));
    expect(sockets("/api/terminals/t1")).toHaveLength(1);
    expect(sockets("/api/terminals/t1")[0].readyState).not.toBe(FakeSocket.CLOSED);
    expect(JSON.parse(localStorage.getItem("foundry.tabs")!)).toEqual(["t1", "t2"]);
  });

  it("closes a tab to its neighbour and then to the board, leaving the terminal running", async () => {
    const calls = mockActions();
    const router = await open("/terminals/t1", live);
    fireEvent.click(screen.getByRole("link", { name: /^shell/ }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t2"));

    fireEvent.click(screen.getByRole("button", { name: "Close fix login / shell" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t1"));
    expect(sockets("/api/terminals/t2")[0].readyState).toBe(FakeSocket.CLOSED);
    fireEvent.click(screen.getByRole("button", { name: "Close fix login / sonnet" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/"));
    expect(screen.queryAllByRole("tab")).toHaveLength(0);
    expect(calls).toEqual([]);
  });

  it("marks a tab in the background whose state changed since you looked, until you look", async () => {
    const router = await open("/terminals/t1", live);
    fireEvent.click(screen.getByRole("link", { name: /^shell/ }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t2"));
    const tab = () => screen.getByRole("tab", { name: /fix login \/ sonnet/ });
    expect(tab().getAttribute("data-changed")).toBeNull();

    const events = sockets("/api/events").at(-1)!;
    const idle: UiMessage = { type: "record", protocol: 1, record: session(ME, "s1", "w1", "t1", "idle", { model: "sonnet", version: 2 }) };
    act(() => events.deliver(JSON.stringify(idle)));
    expect(tab().getAttribute("data-changed")).toBe("true");
    expect(within(tab()).getByText("Idle")).toBeDefined();

    fireEvent.click(tab());
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t1"));
    expect(tab().getAttribute("data-changed")).toBeNull();
  });

  it("drops the tab of a terminal that is gone, as when its workspace is archived", async () => {
    const router = await open("/terminals/t1", live);
    fireEvent.click(screen.getByRole("link", { name: /^shell/ }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t2"));
    fireEvent.click(screen.getByRole("tab", { name: /fix login \/ sonnet/ }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t1"));

    const events = sockets("/api/events").at(-1)!;
    const gone: UiMessage = { type: "record", protocol: 1, record: terminal(ME, "t2", "w1", "exited", { deleted: true, version: 2 }) };
    act(() => events.deliver(JSON.stringify(gone)));
    expect(screen.getAllByRole("tab").map((t) => t.textContent)).toEqual([expect.stringContaining("fix login / sonnet")]);
    expect(sockets("/api/terminals/t2")[0].readyState).toBe(FakeSocket.CLOSED);
  });

  it("opens the open terminal's workspace changes from its header, and the header then shows their size", async () => {
    const calls = mockFetch(
      json(200, {
        base: "main",
        commits: [],
        files: [{ path: "a.ts", from: null, status: "modified", added: 4, removed: 2, uncommitted: true, patch: "@@ -1 +1 @@\n-a\n+b\n" }],
      }),
    );
    await open("/terminals/t1", live);
    const header = screen.getByLabelText("Open terminal");
    fireEvent.click(within(header).getByRole("button", { name: /changes/i }));

    const panel = await screen.findByRole("dialog", { name: "Changes in fix login" });
    await within(panel).findByRole("region", { name: "a.ts" });
    expect(calls[0].url).toBe("/api/workspaces/w1/changes");

    fireEvent.click(within(panel).getByRole("button", { name: "Close changes" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(within(header).getByRole("button", { name: /changes/i }).textContent).toContain("+4");
  });

  it("opens the open session's steps beside its terminal, follows the terminal you open, and closes", async () => {
    const calls = mockActions(json(200, { steps: [{ kind: "prompt", text: "fix the login bug", at: Date.now() }, { kind: "tool", text: "Edit src/login.ts", at: Date.now() }] }));
    const router = await open("/terminals/t1", live);
    fireEvent.click(within(screen.getByLabelText("Open terminal")).getByRole("button", { name: "Steps" }));

    const steps = screen.getByRole("complementary", { name: "Steps" });
    expect(await within(steps).findByText("src/login.ts", { exact: false })).toBeDefined();
    expect(calls[0].url).toBe("/api/sessions/s1/trail");
    expect(within(screen.getByLabelText("Open terminal")).getByRole("button", { name: "Steps" }).getAttribute("aria-pressed")).toBe("true");

    await router.navigate({ to: "/terminals/$terminalId", params: { terminalId: "t2" } });
    const shell = () => screen.getByRole("complementary", { name: "Steps" });
    await waitFor(() => expect(within(shell()).getByText(/shell has no hooks/i)).toBeDefined());
    expect(within(screen.getByLabelText("Open terminal")).getByRole("button", { name: "Steps" })).toBeDefined();

    fireEvent.click(within(shell()).getByRole("button", { name: "Close steps" }));
    expect(screen.queryByRole("complementary", { name: "Steps" })).toBeNull();
  });

  it("opens a workspace's changes from its row in the sidebar", async () => {
    mockFetch(json(200, { base: "main", commits: [], files: [] }));
    await open("/", live);
    const row = screen.getByRole("group", { name: "fix login" });
    fireEvent.click(within(row).getByRole("button", { name: "Changes" }));
    const panel = await screen.findByRole("dialog", { name: "Changes in fix login" });
    await waitFor(() => expect(panel.textContent).toMatch(/no changes since main/i));
  });

  it("brings back the tabs this browser had open without attaching them until one is shown", async () => {
    localStorage.setItem("foundry.tabs", JSON.stringify(["t1", "t2"]));
    await open("/", live);

    expect(screen.getAllByRole("tab")).toHaveLength(2);
    expect(sockets("/api/terminals/t1")).toHaveLength(0);
    expect(sockets("/api/terminals/t2")).toHaveLength(0);
  });
  const three: UiMessage & { type: "state" } = { ...live, records: [...live.records, terminal(ME, "t3", "w1", "running")] };

  it("opens a second terminal beside the open one, and both stay attached and headed", async () => {
    const router = await open("/terminals/t1", live);
    fireEvent.click(within(screen.getByLabelText("Open terminal")).getByRole("button", { name: "Open beside" }));
    const picker = await screen.findByRole("dialog", { name: "Open beside fix login / sonnet" });
    expect(within(picker).getAllByRole("option").map((o) => o.textContent)).toEqual([
      expect.stringContaining("fix login / shell"),
      expect.stringContaining("New shell in fix login"),
    ]);
    fireEvent.keyDown(within(picker).getByRole("combobox"), { key: "Enter" });

    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t2"));
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(within(screen.getByLabelText("Open terminal")).getByText("shell")).toBeDefined();
    expect(within(screen.getByLabelText("Terminal beside")).getByText("sonnet")).toBeDefined();
    expect(sockets("/api/terminals/t1")[0].readyState).not.toBe(FakeSocket.CLOSED);
    expect(sockets("/api/terminals/t2")).toHaveLength(1);
    expect(screen.getAllByRole("tab").map((t) => t.getAttribute("data-beside"))).toEqual(["true", null]);
  });

  it("moves focus to the pane you press in, shows a tab you pick in the focused side, and closes a side to one", async () => {
    const router = await open("/terminals/t1", three);
    fireEvent.keyDown(window, { key: "k", metaKey: true });
    const palette = await screen.findByRole("dialog", { name: "Jump to" });
    fireEvent.change(within(palette).getByRole("combobox"), { target: { value: "shell" } });
    fireEvent.keyDown(within(palette).getByRole("combobox"), { key: "Enter", shiftKey: true });
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t2"));
    expect(screen.getByLabelText("Terminal beside")).toBeDefined();

    fireEvent.pointerDown(screen.getByTestId("pane-t1"));
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t1"));
    expect(within(screen.getByLabelText("Terminal beside")).getByText("shell")).toBeDefined();

    fireEvent.keyDown(window, { key: "k", metaKey: true });
    const again = await screen.findByRole("dialog", { name: "Jump to" });
    const input = within(again).getByRole("combobox");
    fireEvent.change(input, { target: { value: "shell" } });
    fireEvent.keyDown(input, { key: "ArrowDown" });
    fireEvent.keyDown(input, { key: "Enter" });
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t3"));
    expect(within(screen.getByLabelText("Terminal beside")).getByText("shell")).toBeDefined();
    expect(screen.queryByText("sonnet", { selector: "header *" })).toBeNull();
    expect(sockets("/api/terminals/t1")[0].readyState).not.toBe(FakeSocket.CLOSED);

    fireEvent.click(within(screen.getByLabelText("Terminal beside")).getByRole("button", { name: "Close pane" }));
    expect(screen.queryByLabelText("Terminal beside")).toBeNull();
    expect(router.state.location.pathname).toBe("/terminals/t3");
  });

  it("toggles the split with Cmd+\\: a picker opens it, the same keys close it", async () => {
    await open("/terminals/t1", live);
    fireEvent.keyDown(window, { key: "\\", metaKey: true });
    const picker = await screen.findByRole("dialog", { name: /^Open beside/ });
    fireEvent.keyDown(within(picker).getByRole("combobox"), { key: "Enter" });
    await waitFor(() => expect(screen.getByLabelText("Terminal beside")).toBeDefined());

    fireEvent.keyDown(window, { key: "\\", metaKey: true });
    expect(screen.queryByLabelText("Terminal beside")).toBeNull();
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("finds text in the open terminal with Cmd+F or the header's Find button, and closes on Escape", async () => {
    await open("/terminals/t1", live);
    fireEvent.keyDown(window, { key: "f", metaKey: true });

    const box = screen.getByRole("searchbox", { name: "Find in sonnet" });
    fireEvent.change(box, { target: { value: "error" } });
    expect(screen.getByRole("status").textContent).toBe("1 of 1");
    fireEvent.keyDown(box, { key: "Escape" });
    expect(screen.queryByRole("searchbox")).toBeNull();

    const find = within(screen.getByLabelText("Open terminal")).getByRole("button", { name: "Find" });
    fireEvent.click(find);
    expect(screen.getByRole("searchbox", { name: "Find in sonnet" })).toBeDefined();
    expect(find.getAttribute("aria-pressed")).toBe("true");
    fireEvent.click(find);
    expect(screen.queryByRole("searchbox")).toBeNull();
  });

  it("leaves Ctrl+F inside the terminal to the program running there", async () => {
    await open("/terminals/t1", live);
    const inside = document.createElement("div");
    inside.className = "xterm";
    document.body.append(inside);
    fireEvent.keyDown(inside, { key: "f", ctrlKey: true });
    expect(screen.queryByRole("searchbox")).toBeNull();
    inside.remove();
  });
});
