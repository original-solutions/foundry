import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRouter, RouterProvider } from "@tanstack/react-router";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { machineQuery, type Briefs, type Shipping, type UiMessage } from "./api";
import { router, routeTree } from "./App";
import { button, pressed } from "./Modal";
import { ME, OTHER, sample, session, settings, terminal, workspace } from "./test-fixtures";
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
 * Like `mockFetch`, but the board's reads of each workspace's changes get an empty answer, the
 * reads of where each branch stands get `shipping`, and the reads of session briefs get `briefs`;
 * all are left out of the calls, so a test sees only the requests it makes.
 */
let briefs: Briefs = {};
const quiet: Shipping = { branch: "b", pushed: false, unpushed: 0, pr: null, unreachable: false };
let shipping: Shipping = quiet;

function mockActions(...answers: Response[]): Call[] {
  const calls: Call[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init: RequestInit = {}) => {
      if (url.endsWith("/changes")) return json(200, { base: "main", commits: [], files: [] });
      if (url.endsWith("/api/briefs")) return json(200, briefs);
      if (url.endsWith("/pr")) return json(200, shipping);
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
  shipping = quiet;
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
    // The UI is served by machine ME.
    const client = new QueryClient();
    client.setQueryData(machineQuery.queryKey, ME);
    render(
      <QueryClientProvider client={client}>
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

  it("says the daemon cannot be reached, instead of showing an empty fleet, when no state ever arrives", async () => {
    mockActions();
    const fresh = createRouter({ routeTree, history: createMemoryHistory({ initialEntries: ["/"] }) });
    await fresh.load();
    render(
      <QueryClientProvider client={new QueryClient()}>
        <RouterProvider router={fresh} />
      </QueryClientProvider>,
    );
    // Before the first answer the page waits as before.
    expect(screen.queryByRole("heading", { name: "Can't reach the daemon" })).toBeNull();

    act(() => sockets("/api/events")[0].end(1006));

    expect(screen.getByRole("heading", { name: "Can't reach the daemon" })).toBeDefined();
    expect(screen.queryByText(/0 workspaces/)).toBeNull();
    expect(screen.queryByRole("button", { name: "New workspace" })).toBeNull();
  });

  it("greys the last state it was sent and says the daemon was lost, until it sends again", async () => {
    mockActions();
    await open("/", live);
    expect(screen.queryByRole("status")).toBeNull();

    act(() => sockets("/api/events").at(-1)!.end(1006));

    expect(screen.getByRole("status").textContent).toContain("Lost the daemon");
    // The last word stays on screen, marked stale.
    expect(screen.getAllByText("fix login").length).toBeGreaterThan(0);
    expect(document.querySelector("[data-stale]")).not.toBeNull();

    await act(() => new Promise((resolve) => setTimeout(resolve, 600)));
    const again = sockets("/api/events").at(-1)!;
    act(() => again.open());
    act(() => again.deliver(JSON.stringify(live)));

    expect(document.querySelector("[data-stale]")).toBeNull();
    expect(screen.getByRole("status").textContent).toContain("Back after");
  });

  it("heads the open terminal with its workspace, its name, its machine and its state", async () => {
    await open("/terminals/t1", live);

    const header = screen.getByLabelText("Open terminal");
    expect(within(header).getByText("fix login")).toBeDefined();
    expect(within(header).getByText("sonnet")).toBeDefined();
    expect(within(header).getByText("studio")).toBeDefined();
    expect(within(header).getByText("Working")).toBeDefined();
  });

  it("gives the header's Changes and Steps buttons the shared button look, Steps with its pressed look", async () => {
    mockActions(json(200, { steps: [] }));
    await open("/terminals/t1", live);
    const header = screen.getByLabelText("Open terminal");
    const missing = (el: Element, look: string) => look.split(" ").filter((c) => !el.classList.contains(c));
    const steps = within(header).getByRole("button", { name: "Steps" });
    expect(missing(within(header).getByRole("button", { name: /changes/i }), button)).toEqual([]);
    expect(missing(steps, `${button} ${pressed}`)).toEqual([]);
    fireEvent.click(steps);
    expect(steps.getAttribute("aria-pressed")).toBe("true");
    expect(missing(steps, `${button} ${pressed}`)).toEqual([]);
  });

  it("names the open session by its first prompt in the header, the tab and the sidebar, and says what it is doing", async () => {
    briefs = { s1: { title: "fix the login bug", now: { kind: "tool", tool: "Edit", text: "Edit src/login.ts" } } };
    mockActions();
    await open("/terminals/t1", live);

    const header = screen.getByLabelText("Open terminal");
    expect(await within(header).findByText("· fix the login bug")).toBeDefined();
    expect(screen.getByRole("tab", { name: /fix the login bug/ })).toBeDefined();
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

  it("on a phone opens on the board, with the workspaces a drawer from its bottom bar whose title leads back home", async () => {
    mockActions();
    const router = await open("/terminals/t1", live);
    const sidebar = () => document.getElementById("sidebar")!.parentElement!;
    // Home on a phone is the board, not the sidebar: the sidebar keeps its place beside it from `md`.
    await act(() => router.navigate({ to: "/" }));
    expect(screen.getByRole("main").className).not.toMatch(/(^| )hidden( |$)/);
    expect(sidebar().className).toMatch(/(^| )hidden md:flex/);

    fireEvent.click(within(screen.getByRole("navigation", { name: "Home" })).getByRole("button", { name: "Workspaces" }));
    expect(sidebar().className).toMatch(/fixed inset-0/);
    fireEvent.click(screen.getByRole("button", { name: "Close workspaces" }));
    expect(sidebar().className).toMatch(/(^| )hidden md:flex/);

    // From a terminal, the drawer's title goes back to the board.
    await act(() => router.navigate({ to: "/terminals/$terminalId", params: { terminalId: "t1" } }));
    fireEvent.click(screen.getByRole("button", { name: "Workspaces" }));
    fireEvent.click(within(sidebar()).getByRole("link", { name: "Foundry" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/"));
    expect(sidebar().className).toMatch(/(^| )hidden md:flex/);
  });

  it("opens the machines view from the board's switch or a machine's name in the sidebar, and its samples draw as they arrive", async () => {
    mockActions();
    const router = await open("/", live);
    fireEvent.click(screen.getByRole("radio", { name: "Machines" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/machines"));
    const card = await screen.findByRole("article", { name: "studio" });
    expect(screen.queryByRole("region", { name: "Working" })).toBeNull();
    expect(within(card).getByText("This machine")).toBeDefined();
    // A newer sample adds a point to the load line, and the machine was heard.
    const events = sockets("/api/events").at(-1)!;
    act(() => events.deliver(JSON.stringify({ type: "record", protocol: 1, record: sample(ME, 2, { load: 0.9 }) })));
    expect(within(card).getByRole("img", { name: /load per core/i }).querySelector("polyline")?.getAttribute("points")?.split(" ")).toHaveLength(2);
    expect(within(card).getByText(/heard just now/)).toBeDefined();

    fireEvent.click(screen.getByRole("radio", { name: "Timeline" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/"));
    expect(await screen.findByRole("region", { name: "Timeline" })).toBeDefined();

    const name = within(screen.getByRole("navigation", { name: "Workspaces" })).getByRole("link", { name: "studio" });
    fireEvent.click(name);
    await waitFor(() => expect(router.state.location.pathname).toBe("/machines"));
    expect(screen.getByRole("region", { name: "Machines" })).toBeDefined();
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
    expect(screen.getAllByRole("tab").map((t) => t.textContent)).toEqual([expect.stringMatching(/^opus/), expect.stringMatching(/^shell/)]);
    expect(screen.getAllByRole("tab")[0].getAttribute("aria-selected")).toBe("true");
  });

  it("opens a fleet with no workspaces on what Claude should work on, and starting a task lands in its session", async () => {
    const calls = mockActions(json(201, { id: "w9" }), json(201, { id: "s9" }));
    const router = await open("/", { type: "state", protocol: 1, records: [sample(ME, 1)], settings: settings() });

    expect(screen.queryByText(/0 workspaces/)).toBeNull();
    fireEvent.change(screen.getByLabelText("What should Claude work on?"), { target: { value: "Add search to the board." } });
    fireEvent.click(screen.getByRole("button", { name: "Start" }));
    await waitFor(() => expect(calls).toHaveLength(2));
    expect(calls[0].body).toEqual({ project: "p1", base: "main", name: "add search to the board" });

    const events = sockets("/api/events").at(-1)!;
    const arrive = (record: ReturnType<typeof workspace>) => act(() => events.deliver(JSON.stringify({ type: "record", protocol: 1, record })));
    arrive(workspace(ME, "w9", "add search to the board"));
    arrive(terminal(ME, "t9", "w9", "running"));
    arrive(session(ME, "s9", "w9", "t9", "working"));

    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t9"));
  });

  it("shows no first-run page before the daemon's state arrives", async () => {
    const fresh = createRouter({ routeTree, history: createMemoryHistory({ initialEntries: ["/"] }) });
    await fresh.load();
    render(
      <QueryClientProvider client={new QueryClient()}>
        <RouterProvider router={fresh} />
      </QueryClientProvider>,
    );
    expect(screen.queryByLabelText("What should Claude work on?")).toBeNull();
    expect(screen.queryByText(/0 workspaces/)).toBeNull();
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

  const two: UiMessage & { type: "state" } = {
    ...live,
    records: [...live.records, workspace(ME, "w2", "add search"), terminal(ME, "t5", "w2", "running"), session(ME, "s5", "w2", "t5", "idle", { model: "opus" })],
  };

  it("tabs the open workspace's terminals with their states, swaps them for another workspace's, and a tab you go back to has not reconnected", async () => {
    const router = await open("/terminals/t1", two);
    const strip = screen.getByRole("tablist", { name: "Terminals in fix login" });
    const tabs = within(strip).getAllByRole("tab");
    expect(tabs.map((t) => t.textContent)).toEqual([expect.stringMatching(/^sonnet/), expect.stringMatching(/^shell/)]);
    expect(tabs[0].getAttribute("aria-selected")).toBe("true");

    fireEvent.click(tabs[1]);
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t2"));
    fireEvent.click(within(screen.getByRole("group", { name: "add search" })).getByRole("link", { name: /^opus/ }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t5"));
    expect(within(screen.getByRole("tablist", { name: "Terminals in add search" })).getAllByRole("tab").map((t) => t.textContent)).toEqual([expect.stringMatching(/^opus/)]);

    fireEvent.click(within(screen.getByRole("group", { name: "fix login" })).getByRole("link", { name: /^sonnet/ }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t1"));
    expect(sockets("/api/terminals/t1")).toHaveLength(1);
    expect(sockets("/api/terminals/t1")[0].readyState).not.toBe(FakeSocket.CLOSED);
    expect(sockets("/api/terminals/t5")[0].readyState).not.toBe(FakeSocket.CLOSED);
    expect(localStorage.getItem("foundry.tabs")).toBeNull();
    expect(within(screen.getByRole("tablist", { name: "Terminals in fix login" })).getAllByRole("button").map((b) => b.getAttribute("aria-label"))).toEqual(["New in fix login"]);
  });

  it("starts a new shell in the open workspace from the tabs' + and opens it", async () => {
    const calls = mockActions(json(201, { id: "t9" }));
    const router = await open("/terminals/t1", live);
    fireEvent.keyDown(within(screen.getByRole("tablist", { name: "Terminals in fix login" })).getByRole("button", { name: "New in fix login" }), { key: "Enter" });
    const menu = await screen.findByRole("menu");
    expect(within(menu).getAllByRole("menuitem").map((item) => item.textContent)).toEqual(["New session…", "New shell"]);
    fireEvent.click(within(menu).getByRole("menuitem", { name: "New shell" }));

    await waitFor(() => expect(calls).toEqual([{ url: "/api/workspaces/w1/shells", method: "POST", body: undefined }]));
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t9"));
  });

  it.each([
    [
      "the tabs' +",
      async () => {
        fireEvent.keyDown(screen.getByRole("button", { name: "New in fix login" }), { key: "Enter" });
        fireEvent.click(within(await screen.findByRole("menu")).getByRole("menuitem", { name: "New session…" }));
      },
    ],
    [
      "a terminal's actions",
      async () => {
        fireEvent.contextMenu(within(screen.getByRole("group", { name: "fix login" })).getByRole("link", { name: /^shell/ }));
        fireEvent.click(within(await screen.findByRole("menu")).getByRole("menuitem", { name: "New session in fix login" }));
      },
    ],
    [
      "the palette",
      async () => {
        fireEvent.keyDown(window, { key: "k", metaKey: true });
        const dialog = await screen.findByRole("dialog", { name: "Jump to" });
        fireEvent.click(within(dialog).getByRole("button", { name: "Actions for fix login / sonnet" }));
        fireEvent.click(within(dialog).getByRole("option", { name: "New session in fix login" }));
      },
    ],
    ["the sidebar", async () => fireEvent.click(within(screen.getByRole("group", { name: "fix login" })).getByRole("button", { name: "New session" }))],
  ])("starts a new session in the workspace from %s and lands in it once it starts", async (_, ask) => {
    const calls = mockActions(json(201, { id: "s9" }));
    const router = await open("/terminals/t1", live);
    await ask();

    const dialog = await screen.findByRole("dialog", { name: "New session in fix login" });
    fireEvent.click(within(dialog).getByRole("button", { name: "Start" }));
    await waitFor(() => expect(calls).toEqual([{ url: "/api/workspaces/w1/sessions", method: "POST", body: { model: "opus" } }]));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(router.state.location.pathname).toBe("/terminals/t1");

    const events = sockets("/api/events").at(-1)!;
    const arrive = (record: ReturnType<typeof terminal>) => act(() => events.deliver(JSON.stringify({ type: "record", protocol: 1, record })));
    arrive(terminal(ME, "t9", "w1", "running"));
    arrive(session(ME, "s9", "w1", "t9", "starting"));
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t9"));
  });

  it("offers no + on another machine's workspace, which only that machine can start terminals in", async () => {
    await open("/terminals/t7", {
      ...live,
      records: [...live.records, sample(OTHER, 1), workspace(OTHER, "w7", "far away"), terminal(OTHER, "t7", "w7", "running"), terminal(OTHER, "t8", "w7", "running")],
    });
    expect(within(screen.getByRole("tablist", { name: "Terminals in far away" })).getAllByRole("tab")).toHaveLength(2);
    expect(screen.queryByRole("button", { name: "New in far away" })).toBeNull();
  });

  it("lets a file be dropped on this machine's terminals only", async () => {
    const router = await open("/terminals/t7", {
      ...live,
      records: [...live.records, sample(OTHER, 1), workspace(OTHER, "w7", "far away"), terminal(OTHER, "t7", "w7", "running")],
    });
    const drag = { types: ["Files"], files: [], items: [{ kind: "file" }] };
    const pane = async (id: string) => within(await screen.findByTestId(`pane-${id}`)).getByTestId("terminal-pane");
    fireEvent.dragEnter(await pane("t7"), { dataTransfer: drag });
    expect(screen.getByText("Files go only to terminals on this machine")).toBeDefined();
    fireEvent.dragLeave(await pane("t7"), { dataTransfer: drag });

    await act(() => router.navigate({ to: "/terminals/$terminalId", params: { terminalId: "t1" } }));
    fireEvent.dragEnter(await pane("t1"), { dataTransfer: drag });
    expect(await screen.findByText(/^Drop to hand .* this file$/)).toBeDefined();
  });

  it("marks a tab in the background whose state changed since you looked, until you look", async () => {
    const router = await open("/terminals/t1", live);
    fireEvent.click(screen.getByRole("link", { name: /^shell/ }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t2"));
    const tab = () => screen.getByRole("tab", { name: /^sonnet/ });
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
    fireEvent.click(screen.getByRole("tab", { name: /^sonnet/ }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t1"));

    const events = sockets("/api/events").at(-1)!;
    const gone: UiMessage = { type: "record", protocol: 1, record: terminal(ME, "t2", "w1", "exited", { deleted: true, version: 2 }) };
    act(() => events.deliver(JSON.stringify(gone)));
    expect(screen.getAllByRole("tab").map((t) => t.textContent)).toEqual([expect.stringMatching(/^sonnet/)]);
    expect(sockets("/api/terminals/t2")[0].readyState).toBe(FakeSocket.CLOSED);
  });

  const ended: UiMessage & { type: "state" } = {
    ...live,
    records: [...live.records, terminal(ME, "t3", "w1", "exited", { exit_code: 0 }), terminal(ME, "t4", "w1", "exited", { exit_code: 2 })],
  };

  it("right-clicking a terminal lists what can be done with it, and clearing an exited shell needs no asking", async () => {
    const calls = mockActions(new Response(null, { status: 204 }));
    await open("/terminals/t1", ended);
    const sidebar = screen.getByRole("group", { name: "fix login" });

    fireEvent.contextMenu(within(sidebar).getByRole("link", { name: /^shell Exited 0/ }));
    const menu = await screen.findByRole("menu");
    expect(within(menu).getAllByRole("menuitem").map((item) => item.textContent)).toEqual([
      "Open",
      "Open beside",
      "Changes in fix login",
      "New session in fix login",
      "New shell in fix login",
      "Clear",
      "Clear 2 ended in fix login",
      "Archive fix login…",
    ]);
    fireEvent.click(within(menu).getByRole("menuitem", { name: "Clear" }));

    await waitFor(() => expect(calls).toEqual([{ url: "/api/terminals/t3/close", method: "POST", body: undefined }]));
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("clears every ended terminal of a workspace at once from its row", async () => {
    const calls = mockActions(new Response(null, { status: 204 }), new Response(null, { status: 204 }));
    await open("/terminals/t1", ended);
    const sidebar = screen.getByRole("group", { name: "fix login" });

    fireEvent.click(within(sidebar).getByRole("button", { name: "Clear 2 ended" }));

    await waitFor(() => expect(calls.map((c) => c.url).sort()).toEqual(["/api/terminals/t3/close", "/api/terminals/t4/close"]));
  });

  it("ends a running session from the header's menu only after asking, then moves to the next tab once it is gone", async () => {
    const calls = mockActions(new Response(null, { status: 204 }));
    const router = await open("/terminals/t1", live);
    fireEvent.click(screen.getByRole("link", { name: /^shell/ }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t2"));
    fireEvent.click(screen.getByRole("tab", { name: /^sonnet/ }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t1"));

    fireEvent.keyDown(within(screen.getByLabelText("Open terminal")).getByRole("button", { name: "More" }), { key: "Enter" });
    const menu = await screen.findByRole("menu");
    expect(within(menu).queryByRole("menuitem", { name: "Open" })).toBeNull();
    fireEvent.click(within(menu).getByRole("menuitem", { name: "End sonnet…" }));
    const dialog = await screen.findByRole("dialog", { name: "End sonnet?" });
    expect(dialog.textContent).toMatch(/middle of a turn/);
    expect(calls).toEqual([]);
    fireEvent.click(within(dialog).getByRole("button", { name: "End" }));
    await waitFor(() => expect(calls).toEqual([{ url: "/api/terminals/t1/close", method: "POST", body: undefined }]));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());

    const events = sockets("/api/events").at(-1)!;
    const gone: UiMessage = { type: "record", protocol: 1, record: terminal(ME, "t1", "w1", "exited", { deleted: true, version: 2 }) };
    act(() => events.deliver(JSON.stringify(gone)));
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t2"));
    expect(within(screen.getByRole("group", { name: "fix login" })).queryByRole("link", { name: /^sonnet/ })).toBeNull();
  });

  it("says why a terminal could not be ended", async () => {
    mockActions(json(404, { error: "no such terminal" }));
    await open("/terminals/t1", ended);
    fireEvent.contextMenu(within(screen.getByRole("group", { name: "fix login" })).getByRole("link", { name: /^shell Exited 2/ }));
    fireEvent.click(within(await screen.findByRole("menu")).getByRole("menuitem", { name: "Clear" }));

    expect((await screen.findByRole("alert")).textContent).toMatch(/no such terminal/);
  });

  it("offers no ending for another machine's terminals, which only that machine can end", async () => {
    const calls = mockActions();
    await open("/terminals/t1", {
      ...ended,
      records: [
        ...ended.records,
        sample(OTHER, 1),
        workspace(OTHER, "w9", "remote job"),
        terminal(OTHER, "t9", "w9", "exited", { exit_code: 0 }),
        terminal(OTHER, "t8", "w9", "running"),
      ],
      settings: settings({ machines: [{ id: ME, name: "studio", address: "127.0.0.1" }, { id: OTHER, name: "attic", address: "10.0.0.2" }] }),
    });
    const remote = screen.getByRole("group", { name: "remote job" });
    expect(within(remote).queryByRole("button", { name: /^Clear/ })).toBeNull();

    for (const row of within(remote).getAllByRole("link", { name: /^shell/ })) {
      fireEvent.contextMenu(row);
      const menu = await screen.findByRole("menu");
      expect(within(menu).getAllByRole("menuitem").map((item) => item.textContent)).toEqual(["Open", "Open beside", "Changes in remote job"]);
      fireEvent.keyDown(menu, { key: "Escape" });
      await waitFor(() => expect(screen.queryByRole("menu")).toBeNull());
    }
    // This machine's own ended terminals can still be cleared.
    expect(within(screen.getByRole("group", { name: "fix login" })).getByRole("button", { name: "Clear 2 ended" })).toBeDefined();
    expect(calls).toEqual([]);
  });

  it("archives a workspace from its board card after showing what stops, then says what it kept", async () => {
    const calls = mockActions(new Response(null, { status: 204 }));
    await open("/", live);
    const card = screen.getByRole("article", { name: "fix login" });

    fireEvent.click(within(card).getByRole("button", { name: "Archive fix login" }));
    const dialog = await screen.findByRole("dialog", { name: "Archive fix login?" });
    expect(within(dialog).getByRole("region", { name: "Stops" }).textContent).toMatch(/sonnet.*Working.*mid-turn/);
    expect(calls).toEqual([]);
    fireEvent.click(within(dialog).getByRole("button", { name: "Archive" }));

    await waitFor(() => expect(calls).toEqual([{ url: "/api/workspaces/w1/archive", method: "POST", body: { force: false } }]));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect((await screen.findByRole("status")).textContent).toMatch(/Archived fix login.*fix-login.*kept/);
  });

  it("shows a card's merged pull request, linked to GitHub, and archives the workspace from it", async () => {
    shipping = {
      branch: "fix-login",
      pushed: true,
      unpushed: 0,
      pr: { number: 7, title: "Fix login", url: "https://github.com/o/r/pull/7", state: "merged", draft: false, checks: "passing", review: "approved" },
      unreachable: false,
    };
    const calls = mockActions();
    await open("/", live);
    const card = screen.getByRole("article", { name: "fix login" });
    const link = await within(card).findByRole("link", { name: /PR #7 merged/ });
    expect(link.getAttribute("href")).toBe("https://github.com/o/r/pull/7");

    fireEvent.click(within(card).getByRole("button", { name: "Archive fix login: its pull request merged" }));
    expect(await screen.findByRole("dialog", { name: "Archive fix login?" })).toBeDefined();
    expect(calls).toEqual([]);
  });

  it("offers archiving only for this machine's workspaces", async () => {
    mockActions();
    await open("/", {
      ...live,
      records: [...live.records, sample(OTHER, 1), workspace(OTHER, "w9", "remote job"), terminal(OTHER, "t9", "w9", "running")],
      settings: settings({ machines: [{ id: ME, name: "studio", address: "127.0.0.1" }, { id: OTHER, name: "attic", address: "10.0.0.2" }] }),
    });
    expect(within(screen.getByRole("article", { name: "remote job" })).queryByRole("button", { name: /^Archive/ })).toBeNull();
    expect(within(screen.getByRole("group", { name: "remote job" })).queryByRole("button", { name: "Archive" })).toBeNull();
    expect(within(screen.getByRole("group", { name: "fix login" })).getByRole("button", { name: "Archive" })).toBeDefined();
  });

  it("does not end or clear anything when the button that opened the menu is let go over that item", async () => {
    const calls = mockActions();
    await open("/terminals/t1", ended);
    const sidebar = screen.getByRole("group", { name: "fix login" });

    // The press opened the menu under the pointer; its release lands on an item it never pressed.
    for (const name of ["Clear", "Clear 2 ended in fix login"]) {
      fireEvent.contextMenu(within(sidebar).getByRole("link", { name: /^shell Exited 0/ }));
      fireEvent.pointerUp(within(await screen.findByRole("menu")).getByRole("menuitem", { name }));
    }
    fireEvent.contextMenu(within(sidebar).getByRole("link", { name: /^sonnet/ }));
    fireEvent.pointerUp(within(await screen.findByRole("menu")).getByRole("menuitem", { name: "End sonnet…" }));
    fireEvent.keyDown(within(screen.getByLabelText("Open terminal")).getByRole("button", { name: "More" }), { key: "Enter" });
    fireEvent.pointerUp(within(await screen.findByRole("menu")).getByRole("menuitem", { name: "End sonnet…" }));

    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(calls).toEqual([]);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("acts on a pick from the palette: Tab lists what can be done with it, typing narrows the list and Enter does it", async () => {
    mockActions();
    await open("/terminals/t1", live);
    fireEvent.keyDown(window, { key: "k", metaKey: true });
    const dialog = await screen.findByRole("dialog", { name: "Jump to" });
    const input = within(dialog).getByRole("combobox");
    fireEvent.change(input, { target: { value: "login shell" } });
    fireEvent.keyDown(input, { key: "Tab" });

    expect(within(dialog).getByRole("listbox", { name: "Actions for fix login / shell" })).toBeDefined();
    expect(input.getAttribute("aria-label")).toBe("Actions for fix login / shell");
    expect((input as HTMLInputElement).value).toBe("");
    expect(within(dialog).getAllByRole("option").map((o) => o.textContent)).toEqual([
      "Open",
      "Open beside",
      "Changes in fix login",
      "New session in fix login",
      "New shell in fix login",
      "End shell…",
      "Archive fix login…",
    ]);

    fireEvent.change(input, { target: { value: "chan" } });
    expect(within(dialog).getAllByRole("option").map((o) => o.textContent)).toEqual(["Changes in fix login"]);
    fireEvent.keyDown(input, { key: "Enter" });

    expect(await screen.findByRole("dialog", { name: /changes in fix login/i })).toBeDefined();
    expect(screen.queryByRole("dialog", { name: "Jump to" })).toBeNull();
  });

  it("goes back from a pick's actions to the list as it was on Escape or on Backspace in an empty box", async () => {
    await open("/terminals/t1", live);
    fireEvent.keyDown(window, { key: "k", metaKey: true });
    const dialog = await screen.findByRole("dialog", { name: "Jump to" });
    const input = within(dialog).getByRole("combobox");
    fireEvent.change(input, { target: { value: "login" } });
    fireEvent.keyDown(input, { key: "ArrowDown" });
    const picked = input.getAttribute("aria-activedescendant");

    fireEvent.keyDown(input, { key: "Tab" });
    expect(within(dialog).getByRole("listbox", { name: /^Actions for/ })).toBeDefined();
    fireEvent.keyDown(input, { key: "Escape" });
    expect(screen.getByRole("dialog", { name: "Jump to" })).toBe(dialog);
    expect((input as HTMLInputElement).value).toBe("login");
    expect(input.getAttribute("aria-activedescendant")).toBe(picked);

    fireEvent.keyDown(input, { key: "Tab" });
    fireEvent.keyDown(input, { key: "Backspace" });
    expect(within(dialog).getByRole("listbox", { name: "Sessions and shells" })).toBeDefined();

    fireEvent.keyDown(input, { key: "Escape" });
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("opens a row's actions from its button, where a session offers its steps and ending it asks first", async () => {
    const calls = mockActions();
    await open("/", live);
    fireEvent.keyDown(window, { key: "k", metaKey: true });
    const dialog = await screen.findByRole("dialog", { name: "Jump to" });
    fireEvent.click(within(dialog).getByRole("button", { name: "Actions for fix login / sonnet" }));

    // Nothing is open, so there is nothing to open beside.
    expect(within(dialog).getAllByRole("option").map((o) => o.textContent)).toEqual([
      "Open",
      "Steps",
      "Changes in fix login",
      "New session in fix login",
      "New shell in fix login",
      "End sonnet…",
      "Archive fix login…",
    ]);
    fireEvent.click(within(dialog).getByRole("option", { name: "End sonnet…" }));

    const ask = await screen.findByRole("dialog", { name: "End sonnet?" });
    expect(ask.textContent).toMatch(/middle of a turn/);
    expect(screen.queryByRole("dialog", { name: "Jump to" })).toBeNull();
    expect(calls).toEqual([]);
  });

  it("starts a new shell in a terminal's workspace from its actions and opens it", async () => {
    const calls = mockActions(json(201, { id: "t9" }));
    const router = await open("/terminals/t1", live);
    fireEvent.keyDown(window, { key: "k", metaKey: true });
    const dialog = await screen.findByRole("dialog", { name: "Jump to" });
    fireEvent.click(within(dialog).getByRole("button", { name: "Actions for fix login / sonnet" }));
    fireEvent.click(within(dialog).getByRole("option", { name: "New shell in fix login" }));

    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t9"));
    expect(calls).toEqual([{ url: "/api/workspaces/w1/shells", method: "POST", body: undefined }]);
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
    const calls = mockActions(json(200, { steps: [{ kind: "prompt", text: "fix the login bug", at: Date.now() }, { kind: "tool", tool: "Edit", text: "Edit src/login.ts", at: Date.now() }] }));
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

  const many: UiMessage & { type: "state" } = {
    ...live,
    records: [...live.records, ...["t3", "t4", "t5", "t6", "t7", "t8"].map((id) => terminal(ME, id, "w1", "running"))],
  };
  const live_ = (id: string) => sockets(`/api/terminals/${id}`).filter((s) => s.readyState !== FakeSocket.CLOSED);
  const visit = async (router: Awaited<ReturnType<typeof open>>, id: string) => {
    await act(() => router.navigate({ to: "/terminals/$terminalId", params: { terminalId: id } }));
    expect(router.state.location.pathname).toBe(`/terminals/${id}`);
  };

  it("keeps only the six terminals used last attached, and one you go back to attaches again with its tab intact", async () => {
    const router = await open("/terminals/t1", many);
    for (const id of ["t2", "t3", "t4", "t5", "t6", "t7"]) await visit(router, id);

    expect(screen.getAllByRole("tab")).toHaveLength(8);
    expect(screen.queryByTestId("pane-t1")).toBeNull();
    expect(live_("t1")).toHaveLength(0);
    for (const id of ["t2", "t3", "t4", "t5", "t6", "t7"]) expect(live_(id)).toHaveLength(1);

    const events = sockets("/api/events").at(-1)!;
    const idle: UiMessage = { type: "record", protocol: 1, record: session(ME, "s1", "w1", "t1", "idle", { model: "sonnet", version: 2 }) };
    act(() => events.deliver(JSON.stringify(idle)));
    const tab = screen.getByRole("tab", { name: /^sonnet/ });
    expect(tab.getAttribute("data-changed")).toBe("true");
    expect(within(tab).getByText("Idle")).toBeDefined();

    await visit(router, "t1");
    expect(sockets("/api/terminals/t1")).toHaveLength(2);
    expect(live_("t1")).toHaveLength(1);
    expect(live_("t2")).toHaveLength(0);
    expect(screen.getByRole("tab", { name: /^sonnet/ }).getAttribute("data-changed")).toBeNull();
  });

  it("never detaches the other side of a split, however many terminals pass through the focused side", async () => {
    const router = await open("/terminals/t1", many);
    fireEvent.keyDown(window, { key: "\\", metaKey: true });
    const picker = await screen.findByRole("dialog", { name: /^Open beside/ });
    fireEvent.keyDown(within(picker).getByRole("combobox"), { key: "Enter" });
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t2"));
    for (const id of ["t3", "t4", "t5", "t6", "t7", "t8"]) await visit(router, id);

    expect(within(screen.getByLabelText("Terminal beside")).getByText("sonnet")).toBeDefined();
    expect(live_("t1")).toHaveLength(1);
    expect(sockets("/api/terminals/t1")).toHaveLength(1);
    expect(screen.getAllByTestId(/^pane-/)).toHaveLength(6);
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

  it("on a narrow screen, where the header has no room for Find, offers it in the header's menu", async () => {
    vi.stubGlobal("matchMedia", () => ({ matches: false }));
    await open("/terminals/t1", live);
    fireEvent.keyDown(within(screen.getByLabelText("Open terminal")).getByRole("button", { name: "More" }), { key: "Enter" });
    const menu = await screen.findByRole("menu");
    expect(within(menu).queryByRole("menuitem", { name: "Open beside" })).toBeNull();

    fireEvent.click(within(menu).getByRole("menuitem", { name: "Find in scrollback" }));
    const box = await screen.findByRole("searchbox", { name: "Find in sonnet" });
    await waitFor(() => expect(screen.queryByRole("menu")).toBeNull());
    expect(document.activeElement).toBe(box);
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
