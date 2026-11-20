import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, RouterProvider } from "@tanstack/react-router";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createContext, useContext } from "react";
import type { AnyRecord, Changes, SessionState } from "./api";
import { Board, BOARD_BUSY_EVERY } from "./Board";
import { fleet, type MachineView } from "./fleet";
import { ME, sample, session, settings, terminal, workspace } from "./test-fixtures";
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
  startOnScreen = true;
  vi.stubGlobal("IntersectionObserver", FakeObserver);
  answers["/api/workspaces/w1/changes"] = () =>
    json(200, {
      base: "main",
      commits: ["abc1234 Check the session on login", "def5678 Add the login form"],
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

async function show(machines: MachineView[], onChanges = vi.fn()) {
  function Shown() {
    return <Board machines={useContext(Machines)} onJump={() => {}} onCreate={() => {}} onChanges={onChanges} />;
  }
  const root = createRootRoute({ component: Shown });
  const router = createRouter({
    routeTree: root.addChildren([createRoute({ getParentRoute: () => root, path: "/terminals/$terminalId" })]),
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
  return { router, update: (next: MachineView[]) => view.rerender(tree(next)) };
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
});
