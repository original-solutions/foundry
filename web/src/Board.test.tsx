import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, RouterProvider } from "@tanstack/react-router";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createContext, useContext, type ComponentProps } from "react";
import type { AnyRecord, Changes, SessionState } from "./api";
import { archiveOf } from "./Archive";
import { Board, BOARD_BUSY_EVERY } from "./Board";
import { fleet, type MachineView } from "./fleet";
import { ME, sample, session, settings, terminal, workspace } from "./test-fixtures";
import { RETRY_EVERY } from "./poll";
import { json } from "./test-utils";

const file = (path: string, added: number, removed: number, uncommitted = false) => ({
  path,
  from: null,
  status: "modified" as const,
  added,
  removed,
  uncommitted,
  patch: "",
});

const records = (state: SessionState = "working"): AnyRecord[] => [
  sample(ME, 1),
  workspace(ME, "w1", "fix login"),
  terminal(ME, "t1", "w1"),
  session(ME, "s1", "w1", "t1", state),
  terminal(ME, "t2", "w1"),
  workspace(ME, "w2", "tidy readme"),
  terminal(ME, "t3", "w2"),
  session(ME, "s3", "w2", "t3", "waiting_for_you", { model: "sonnet" }),
  workspace(ME, "w3", "far away"),
  terminal(ME, "t4", "w3"),
  session(ME, "s4", "w3", "t4", "idle"),
];

const answers: Record<string, () => Response> = {};
let reads: string[] = [];
const readsOf = (workspace: string) => reads.filter((u) => u.includes(`/${workspace}/`)).length;

/** The cards the board watches, and whether a card starts on screen. */
let watched: { element: Element; seen: (entries: Partial<IntersectionObserverEntry>[]) => void }[] = [];
let startOnScreen = true;
class FakeObserver {
  constructor(private seen: (entries: Partial<IntersectionObserverEntry>[]) => void) {}
  observe(element: Element) {
    watched.push({ element, seen: this.seen });
    if (startOnScreen) this.seen([{ target: element, isIntersecting: true }]);
  }
  unobserve() {}
  disconnect() {
    watched = watched.filter((w) => w.seen !== this.seen);
  }
}
/** Scrolls the card for `name` onto or off the screen. */
function scroll(name: string, onScreen: boolean) {
  const element = card(name);
  act(() => {
    for (const w of watched.filter((w) => w.element === element)) w.seen([{ target: element, isIntersecting: onScreen }]);
  });
}

beforeEach(() => {
  reads = [];
  watched = [];
  localStorage.clear();
  startOnScreen = true;
  vi.stubGlobal("IntersectionObserver", FakeObserver);
  answers["/api/workspaces/w1/changes"] = () =>
    json(200, {
      base: "main",
      commits: [
        { hash: "abc1234", subject: "Check the session on login" },
        { hash: "def5678", subject: "Add the login form" },
      ],
      files: [file("src/login.ts", 30, 4), file("README.md", 2, 0, true), file("src/a.ts", 1, 1), file("src/b.ts", 1, 0)],
    } satisfies Changes);
  answers["/api/workspaces/w2/changes"] = () => json(200, { base: "main", commits: [], files: [] } satisfies Changes);
  answers["/api/workspaces/w3/changes"] = () => json(404, { error: "workspace not found" });
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      reads.push(url);
      return answers[url]();
    }),
  );
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

const Machines = createContext<MachineView[]>([]);

async function show(machines: MachineView[], onChanges = vi.fn(), props: Partial<ComponentProps<typeof Board>> = {}) {
  function Shown() {
    return (
      <Board
        machines={useContext(Machines)}
        projects={settings().projects}
        onJump={() => {}}
        onChanges={onChanges}
        onSteps={() => {}}
        onCreated={() => {}}
        onMore={() => {}}
        traced={new Set()}
        onMenu={() => {}}
        {...props}
      />
    );
  }
  const root = createRootRoute({ component: Shown });
  const router = createRouter({
    routeTree: root.addChildren([createRoute({ getParentRoute: () => root, path: "/terminals/$terminalId" }), createRoute({ getParentRoute: () => root, path: "/machines" })]),
    history: createMemoryHistory({ initialEntries: ["/"] }),
  });
  await router.load();
  const client = new QueryClient();
  const tree = (list: MachineView[]) => (
    <Machines.Provider value={list}>
      <QueryClientProvider client={client}>
        <RouterProvider router={router} />
      </QueryClientProvider>
    </Machines.Provider>
  );
  const view = render(tree(machines));
  return { router, client, update: (next: MachineView[]) => view.rerender(tree(next)) };
}

const card = (name: string) => screen.getByRole("article", { name });

describe("Board", () => {
  it("shows every workspace as a card in a lane by what it needs: you, its agents, or nothing", async () => {
    await show(fleet(records(), settings()));
    const needs = screen.getByRole("region", { name: "Needs you" });
    const working = screen.getByRole("region", { name: "Working" });
    const quiet = screen.getByRole("region", { name: "Quiet" });
    expect(within(needs).getByRole("article", { name: "tidy readme" })).toBeDefined();
    expect(within(working).getByRole("article", { name: "fix login" })).toBeDefined();
    expect(within(quiet).getByRole("article", { name: "far away" })).toBeDefined();
  });

  it("lists a card's terminals with their states, each opening its terminal", async () => {
    const { router } = await show(fleet(records(), settings()));
    const login = card("fix login");
    expect(within(login).getByText("Working")).toBeDefined();
    expect(within(login).getByText("Running")).toBeDefined();
    fireEvent.click(within(login).getByRole("link", { name: /^shell/ }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t2"));
  });

  it("shows what each workspace changed: lines, files, commits, the biggest files and the newest commit", async () => {
    await show(fleet(records(), settings()));
    const login = card("fix login");
    await waitFor(() => expect(within(login).getByText("+34")).toBeDefined());
    expect(within(login).getByText("−5")).toBeDefined();
    expect(within(login).getByText("4 files")).toBeDefined();
    expect(within(login).getByText("2 commits")).toBeDefined();
    expect(within(login).getByText("1 uncommitted")).toBeDefined();
    expect(within(login).getByText("login.ts")).toBeDefined();
    expect(within(login).getByText("+1 more")).toBeDefined();
    expect(within(login).getByText("abc1234")).toBeDefined();
    expect(within(login).getByText("Check the session on login")).toBeDefined();
    await waitFor(() => expect(within(card("tidy readme")).getByText("No changes yet")).toBeDefined());
    await waitFor(() => expect(within(card("far away")).getByText(/not readable from here/i)).toBeDefined());
  });

  it("opens a workspace's changes from its card", async () => {
    const onChanges = vi.fn();
    await show(fleet(records(), settings()), onChanges);
    const login = card("fix login");
    await waitFor(() => expect(within(login).getByText("+34")).toBeDefined());
    fireEvent.click(within(login).getByRole("button", { name: /changes/i }));
    expect(onChanges).toHaveBeenCalledWith("w1");
  });

  it("reads a workspace's changes again when a terminal's state changes, as when a turn ends", async () => {
    const { update } = await show(fleet(records("waiting_for_you"), settings()));
    await waitFor(() => expect(within(card("fix login")).getByText("+34")).toBeDefined());
    const before = reads.filter((u) => u.includes("/w1/")).length;
    update(fleet(records("failed"), settings()));
    await waitFor(() => expect(reads.filter((u) => u.includes("/w1/")).length).toBe(before + 1));
  });

  it("reads a card's changes only once the card is on screen", async () => {
    startOnScreen = false;
    await show(fleet(records(), settings()));
    await act(async () => {});
    expect(reads).toEqual([]);
    expect(within(card("fix login")).getByText(/reading changes/i)).toBeDefined();
    scroll("fix login", true);
    await waitFor(() => expect(within(card("fix login")).getByText("+34")).toBeDefined());
    expect(readsOf("w1")).toBe(1);
    expect(readsOf("w2")).toBe(0);
  });

  it("reads a card again on a timer only while an agent in it works, and never a quiet one", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    await show(fleet(records("working"), settings()));
    await waitFor(() => expect(within(card("tidy readme")).getByText("No changes yet")).toBeDefined());
    const working = readsOf("w1");
    const waiting = readsOf("w2");
    const twoMinutes = 120_000;
    await act(() => vi.advanceTimersByTimeAsync(twoMinutes + 100));
    expect(readsOf("w1")).toBe(working + Math.floor(twoMinutes / BOARD_BUSY_EVERY));
    expect(readsOf("w2")).toBe(waiting);
  });

  it("keeps reading a working card after a failed read, and stops for a workspace the daemon does not have", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    answers["/api/workspaces/w1/changes"] = () => json(500, { error: "git hiccup" });
    const allWorking = records().map((r) => (r.kind === "session" ? { ...r, body: { ...r.body, state: "working" as const } } : r));
    await show(fleet(allWorking, settings()));
    await waitFor(() => expect(within(card("far away")).getByText(/not readable from here/i)).toBeDefined());
    const twoMinutes = 120_000;
    await act(() => vi.advanceTimersByTimeAsync(twoMinutes + 100));
    expect(readsOf("w1")).toBe(1 + Math.floor(twoMinutes / Math.max(BOARD_BUSY_EVERY, RETRY_EVERY)));
    expect(readsOf("w3")).toBe(1);
  });

  it("reads a card whose terminal changed state off screen when it comes back on screen, not before", async () => {
    const { update } = await show(fleet(records("waiting_for_you"), settings()));
    await waitFor(() => expect(within(card("fix login")).getByText("+34")).toBeDefined());
    const before = readsOf("w1");
    scroll("fix login", false);
    update(fleet(records("failed"), settings()));
    await act(async () => {});
    expect(readsOf("w1")).toBe(before);
    scroll("fix login", true);
    await waitFor(() => expect(readsOf("w1")).toBe(before + 1));
  });

  it("stops reading a card's changes and shows none once its workspace is archiving or archived", async () => {
    let archived: (done: Response) => void = () => {};
    const archive = new Promise<Response>((resolve) => (archived = resolve));
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) => {
        reads.push(url);
        return url.endsWith("/archive") ? archive : answers[url]();
      }),
    );
    // Mid-archive, a read sees the worktree's files go: every file deleted.
    const { client, update } = await show(fleet(records("waiting_for_you"), settings()));
    const login = card("fix login");
    await waitFor(() => expect(within(login).getByText("+34")).toBeDefined());
    answers["/api/workspaces/w1/changes"] = () => json(200, { base: "main", commits: [], files: [file("src/login.ts", 0, 24986)] } satisfies Changes);
    const before = readsOf("w1");

    void client.getMutationCache().build(client, archiveOf("w1")).execute(true);
    await waitFor(() => expect(within(login).getByText(/archiving/i)).toBeDefined());
    expect(within(login).queryByText("+34")).toBeNull();
    // The archive closes its terminals.
    const closed = records("waiting_for_you").filter((r) => !(r.kind === "session" && r.body.workspace === "w1") && !(r.kind === "terminal" && r.body.workspace === "w1"));
    update(fleet(closed, settings()));
    await act(async () => {});

    archived(new Response(null, { status: 204 }));
    await act(async () => {});
    update(fleet(records("failed"), settings()));
    await act(async () => {});
    expect(readsOf("w1") - before).toBe(1); // the archive itself
    expect(within(card("fix login")).queryByText(/24986/)).toBeNull();
    expect(within(card("fix login")).getByText(/archiving/i)).toBeDefined();
  });

  it("switches to a timeline of every session's turns, and keeps that view for next time", async () => {
    await show(fleet(records(), settings()));
    fireEvent.click(screen.getByRole("radio", { name: "Timeline" }));
    expect(await screen.findByRole("region", { name: "Timeline" })).toBeDefined();
    expect(screen.queryByRole("region", { name: "Working" })).toBeNull();
    expect(localStorage.getItem("foundry.home")).toBe("timeline");
    fireEvent.click(screen.getByRole("radio", { name: "Board" }));
    expect(screen.getByRole("region", { name: "Working" })).toBeDefined();
    expect(localStorage.getItem("foundry.home")).toBe("board");
  });
});

describe("On a phone", () => {
  it("says at the top what needs you, what works and what rests", async () => {
    await show(fleet(records(), settings()));
    expect(screen.getByText("1 needs you · 1 working · 1 quiet")).toBeDefined();
  });

  it("switches the view from a bar at the bottom, which counts what needs you and opens the workspaces", async () => {
    const onMenu = vi.fn();
    const { router } = await show(fleet(records(), settings()), vi.fn(), { onMenu });
    const bar = screen.getByRole("navigation", { name: "Home" });
    const board = within(bar).getByRole("button", { name: "Board, 1 needs you" });
    expect(board.getAttribute("aria-current")).toBe("page");

    fireEvent.click(within(bar).getByRole("button", { name: "Timeline" }));
    expect(await screen.findByRole("region", { name: "Timeline" })).toBeDefined();
    expect(localStorage.getItem("foundry.home")).toBe("timeline");
    expect(within(bar).getByRole("button", { name: "Timeline" }).getAttribute("aria-current")).toBe("page");

    fireEvent.click(within(bar).getByRole("button", { name: "Workspaces" }));
    expect(onMenu).toHaveBeenCalledOnce();

    fireEvent.click(within(bar).getByRole("button", { name: "Machines" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/machines"));
  });
});

describe("The task bar", () => {
  const bar = () => screen.getByRole("textbox", { name: "New task" });
  const start = () => screen.getByRole("button", { name: /^Start/ }) as HTMLButtonElement;
  /** Answers the board's reads as before, and the create's two posts, keeping what each post sent. */
  function creating() {
    const posts: { url: string; body: unknown }[] = [];
    answers["/api/workspaces"] = () => json(201, { id: "w9" });
    answers["/api/workspaces/w9/sessions"] = () => json(201, { id: "s9" });
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init: RequestInit = {}) => {
        if (init.method === "POST") posts.push({ url, body: JSON.parse(String(init.body)) });
        else reads.push(url);
        return answers[url]();
      }),
    );
    return posts;
  }

  it("is one line with a way to jump until it is used, then shows where the task will run", async () => {
    const jump = vi.fn();
    await show(fleet(records(), settings()), vi.fn(), { onJump: jump });

    expect(bar().getAttribute("placeholder")).toBe("Give Claude a new task…");
    expect(screen.queryByRole("radio", { name: "demo" })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Jump to a session or shell" }));
    expect(jump).toHaveBeenCalled();

    fireEvent.focus(bar());
    expect(screen.getByRole("radio", { name: "demo" })).toBeDefined();
    expect(start().disabled).toBe(true);
    fireEvent.blur(bar());
    expect(screen.queryByRole("radio", { name: "demo" })).toBeNull();

    fireEvent.change(bar(), { target: { value: "Add a footer" } });
    fireEvent.blur(bar());
    expect(screen.getByLabelText("Branch").textContent).toBe("add-a-footer");
  });

  it("makes a workspace for the task and starts Claude on it, then hands over the session", async () => {
    const posts = creating();
    const created = vi.fn();
    await show(fleet(records(), settings()), vi.fn(), { onCreated: created });

    fireEvent.focus(bar());
    fireEvent.change(bar(), { target: { value: "Add a footer.\nKeep it small." } });
    fireEvent.keyDown(bar(), { key: "Enter", metaKey: true });

    await waitFor(() => expect(created).toHaveBeenCalledWith("s9"));
    expect(posts).toEqual([
      { url: "/api/workspaces", body: { project: "p1", base: "main", name: "add a footer" } },
      { url: "/api/workspaces/w9/sessions", body: { model: "opus", prompt: "Add a footer.\nKeep it small." } },
    ]);
  });

  it("numbers a name the project already has a workspace for, and refuses a typed one", async () => {
    const posts = creating();
    await show(fleet(records(), settings()));

    fireEvent.focus(bar());
    fireEvent.change(bar(), { target: { value: "Fix login" } });
    expect(screen.getByLabelText("Branch").textContent).toBe("fix-login-2");

    fireEvent.change(screen.getByLabelText("Workspace name"), { target: { value: "Tidy README" } });
    expect(screen.getByText("demo already has a workspace on tidy-readme. Pick another name.")).toBeDefined();
    expect(start().disabled).toBe(true);
    fireEvent.keyDown(bar(), { key: "Enter", metaKey: true });
    await act(async () => {});
    expect(posts).toEqual([]);
  });
});
