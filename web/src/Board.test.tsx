import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, RouterProvider } from "@tanstack/react-router";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createContext, useContext, type ComponentProps } from "react";
import type { AnyRecord, Changes, SessionState, Shipping } from "./api";
import { archiveOf } from "./Archive";
import { buttonShape, pressed } from "./Modal";
import { Board, BOARD_BUSY_EVERY } from "./Board";
import { fleet, withFactory, type MachineView } from "./fleet";
import { factoryStatus, factoryStatuses, ME, onCommit, OTHER, sample, session, settings, terminal, workspace } from "./test-fixtures";
import { RETRY_EVERY } from "./poll";
import { TerminalActions } from "./TerminalMenu";
import { expectButtonShape, FakeSocket, json } from "./test-utils";

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

async function show(machines: MachineView[], onChanges = vi.fn(), props: Partial<ComponentProps<typeof Board>> = {}, served = false) {
  function Shown() {
    const machines = useContext(Machines);
    const board = (
      <Board
        machines={machines}
        projects={settings().projects}
        onJump={() => {}}
        onChanges={onChanges}
        onSteps={() => {}}
        onCreated={() => {}}
        traced={new Set()}
        onMenu={() => {}}
        history={{}}
        connected
        {...props}
      />
    );
    if (!served) return board;
    // Served by a daemon, as in the app: the board knows its machine and can act on its terminals.
    const noop = () => {};
    return (
      <TerminalActions machines={machines} onOpen={noop} onBeside={noop} onSteps={noop} onChanges={noop} onFind={noop} onNewSession={noop}>
        {board}
      </TerminalActions>
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
  it("marks each card with its project's face beside the project's name", async () => {
    await show(fleet(records(), settings()));
    expect(within(card("fix login")).getByTitle("demo").textContent).toBe("DE");
  });

  it("shows every workspace as a card in a lane by what it needs: you, its agents, or nothing", async () => {
    await show(fleet(records(), settings()));
    const needs = screen.getByRole("region", { name: "Needs you" });
    const working = screen.getByRole("region", { name: "Working" });
    const quiet = screen.getByRole("region", { name: "Quiet" });
    expect(within(needs).getByRole("article", { name: "tidy readme" })).toBeDefined();
    expect(within(working).getByRole("article", { name: "fix login" })).toBeDefined();
    expect(within(quiet).getByRole("article", { name: "far away" })).toBeDefined();
  });

  it("lists no workspace of a machine the machine list does not hold, nor its terminals or sessions", async () => {
    const all = [
      ...records(),
      sample(OTHER, 1),
      workspace(OTHER, "w9", "cut off"),
      terminal(OTHER, "t9", "w9"),
      session(OTHER, "s9", "w9", "t9", "waiting_for_you", { model: "gone-model" }),
    ];
    await show(fleet(all, settings({ machines: [{ id: ME, name: "studio", address: "" }] })));

    expect(card("fix login")).toBeDefined();
    expect(screen.queryByRole("article", { name: "cut off" })).toBeNull();
    expect(screen.queryByText("gone-model")).toBeNull();
  });

  it("lists a card's terminals with their states, each opening its terminal", async () => {
    const { router } = await show(fleet(records(), settings()));
    const login = card("fix login");
    expect(within(login).getByText("Working")).toBeDefined();
    expect(within(login).getByText("Open")).toBeDefined();
    fireEvent.click(within(login).getByRole("link", { name: /^shell/ }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t2"));
  });

  it("shows under an idle session what Claude said at the end of its turn, and nothing under a working one", async () => {
    const said = "Fixed the login check. All tests pass.";
    const briefs = { s1: { title: null, now: null, said: "left over" }, s4: { title: null, now: null, said } };
    await show(fleet(records(), settings(), briefs));
    expect(within(card("far away")).getByRole("button", { name: said })).toBeDefined();
    expect(within(card("fix login")).queryByText("left over")).toBeNull();
    fireEvent.click(within(card("far away")).getByRole("button", { name: said }));
    expect(within(card("far away")).getByRole("textbox", { name: "Reply to opus" })).toBeDefined();
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
    await waitFor(() => expect(within(card("far away")).getByText(/can.t be read from here/i)).toBeDefined());
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
    await waitFor(() => expect(within(card("far away")).getByText(/can.t be read from here/i)).toBeDefined());
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
    // Three views: the board, the timeline and the machines; where work stands is a grouping of the board.
    expect(within(screen.getByRole("radiogroup", { name: "View" })).getAllByRole("radio").map((r) => r.textContent)).toEqual(["Board", "Timeline", "Machines"]);
    fireEvent.click(screen.getByRole("radio", { name: "Board" }));
    expect(screen.getByRole("region", { name: "Working" })).toBeDefined();
    expect(localStorage.getItem("foundry.home")).toBe("board");
  });
});

describe("As a list", () => {
  const asList = () => fireEvent.click(within(screen.getByRole("group", { name: "Show workspaces as" })).getByRole("button", { name: "List" }));

  it("shows each workspace as one line in the same lanes, and keeps the choice for next time", async () => {
    await show(fleet(records(), settings()));
    asList();
    const needs = screen.getByRole("region", { name: "Needs you" });
    const line = within(needs).getByRole("article", { name: "tidy readme" });
    expect(within(line).getByRole("link", { name: "tidy readme" }).getAttribute("href")).toBe("/terminals/t3");
    expect(within(line).getByText("Needs you")).toBeDefined();
    expect(within(screen.getByRole("region", { name: "Working" })).getByRole("article", { name: "fix login" })).toBeDefined();
    expect(within(screen.getByRole("group", { name: "Show workspaces as" })).getByRole("button", { name: "List" }).getAttribute("aria-pressed")).toBe("true");
    expect(localStorage.getItem("foundry.layout")).toBe("list");
    // The switch is drawn as Group by's is: the shared button with its pressed look.
    for (const each of within(screen.getByRole("group", { name: "Show workspaces as" })).getAllByRole("button")) {
      expect(`${buttonShape} ${pressed}`.split(" ").filter((c) => !each.classList.contains(c))).toEqual([]);
    }
    // A line is one row: no list of its terminals under it.
    expect(within(card("fix login")).queryByRole("list")).toBeNull();
  });

  it("marks each line with its project's face, as a card is", async () => {
    await show(fleet(records(), settings()));
    asList();
    expect(within(card("fix login")).getByTitle("demo").textContent).toBe("DE");
  });

  it("opens each terminal from its glyph and the changes from the line's count", async () => {
    const onChanges = vi.fn();
    const { router } = await show(fleet(records(), settings()), onChanges);
    asList();
    const login = card("fix login");
    fireEvent.click(within(login).getByRole("link", { name: "shell Open" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/terminals/t2"));
    await waitFor(() => expect(within(login).getByText("+34")).toBeDefined());
    expect(within(login).getByText("−5")).toBeDefined();
    expect(within(login).getByText("1 uncommitted")).toBeDefined();
    fireEvent.click(within(login).getByRole("button", { name: "Changes in fix login" }));
    expect(onChanges).toHaveBeenCalledWith("w1");
    await waitFor(() => expect(within(card("tidy readme")).getByText("No changes")).toBeDefined());
  });

  it("keeps a line's glyphs to three, and with more shows two and how many others", async () => {
    await show(fleet([...records(), terminal(ME, "t5", "w1"), terminal(ME, "t6", "w1")], settings()));
    asList();
    const login = card("fix login");
    expect(within(login).getAllByRole("link", { name: /Open|Working/ })).toHaveLength(2);
    expect(within(login).getByText("+2")).toBeDefined();
    expect(within(card("tidy readme")).queryByText(/^\+\d$/)).toBeNull();
  });

  it("says on the line what its first terminal is doing: the question, the tool, or the first line Claude said", async () => {
    const briefs = {
      s1: { title: null, now: { kind: "tool" as const, text: "Edit src/login.ts" }, said: undefined },
      s3: { title: null, now: { kind: "ask" as const, text: "May I run the migration?" } },
      s4: { title: null, now: null, said: "Fixed the login check.\nAll tests pass." },
    };
    await show(fleet(records(), settings(), briefs));
    asList();
    expect(within(card("fix login")).getByText("Edit src/login.ts")).toBeDefined();
    expect(within(card("tidy readme")).getByText("May I run the migration?")).toBeDefined();
    const said = within(card("far away")).getByText("Fixed the login check.");
    expect(said.getAttribute("title")).toBe("Fixed the login check.\nAll tests pass.");
  });

  it("reads a line's changes only once it is on screen, as a card does", async () => {
    startOnScreen = false;
    localStorage.setItem("foundry.layout", "list");
    await show(fleet(records(), settings()));
    await act(async () => {});
    expect(reads).toEqual([]);
    scroll("fix login", true);
    await waitFor(() => expect(within(card("fix login")).getByText("+34")).toBeDefined());
    expect(readsOf("w2")).toBe(0);
  });

  it("opens as a list for a browser that kept it, and goes back to cards", async () => {
    localStorage.setItem("foundry.layout", "list");
    await show(fleet(records(), settings()));
    expect(within(card("fix login")).queryByText("Open")).toBeNull();
    fireEvent.click(within(screen.getByRole("group", { name: "Show workspaces as" })).getByRole("button", { name: "Cards" }));
    expect(within(card("fix login")).getByText("Open")).toBeDefined();
    expect(localStorage.getItem("foundry.layout")).toBe("cards");
  });
});

describe("Grouped by shipping", () => {
  const commit = (hash: string, subject: string) => ({ hash, subject });
  const shipping = (extra: Partial<Shipping> = {}): Shipping => ({ branch: "b", pushed: false, unpushed: 0, pr: null, unreachable: false, ...extra });
  const pr = (number: number, state: "open" | "merged", checks: "failing" | "passing") => ({ number, title: `PR ${number}`, url: `https://github.com/o/r/pull/${number}`, state, draft: false, checks, review: "none" as const });
  const fleetOf = (draft: SessionState = "idle"): AnyRecord[] => [
    sample(ME, 1),
    sample(OTHER, 1),
    workspace(ME, "w1", "draft login"),
    terminal(ME, "t1", "w1"),
    session(ME, "s1", "w1", "t1", draft),
    workspace(ME, "w2", "push docs"),
    terminal(ME, "t2", "w2"),
    session(ME, "s2", "w2", "t2", "working"),
    workspace(ME, "w3", "fix footer"),
    terminal(ME, "t3", "w3"),
    session(ME, "s3", "w3", "t3", "idle"),
    workspace(ME, "w4", "rename api"),
    terminal(ME, "t4", "w4"),
    workspace(ME, "w5", "say hi"),
    terminal(ME, "t5", "w5"),
    workspace(OTHER, "w6", "far off"),
    terminal(OTHER, "t6", "w6"),
  ];
  const lane = (name: string) => screen.getByRole("region", { name });
  // "far off" lives on a listed machine; one the list does not hold shows no workspaces at all.
  const listed = () => settings({ machines: [{ id: ME, name: "studio", address: "" }, { id: OTHER, name: "laptop", address: "" }] });

  beforeEach(() => {
    FakeSocket.all = [];
    vi.stubGlobal("WebSocket", FakeSocket);
    answers["/api/machine"] = () => json(200, { id: ME });
    answers["/api/workspaces/w1/changes"] = () => json(200, { base: "main", commits: [commit("aaa1111", "Draft the login form")], files: [file("src/login.ts", 40, 2), file("README.md", 3, 0, true)] } satisfies Changes);
    answers["/api/workspaces/w1/pr"] = () => json(200, shipping({ unpushed: 1 }));
    answers["/api/workspaces/w2/changes"] = () => json(200, { base: "main", commits: [commit("bbb2222", "Document the API")], files: [file("docs/api.md", 12, 1)] } satisfies Changes);
    answers["/api/workspaces/w2/pr"] = () => json(200, shipping({ pushed: true }));
    answers["/api/workspaces/w3/changes"] = () => json(200, { base: "main", commits: [commit("ccc3333", "Show the version")], files: [file("src/footer.tsx", 5, 1)] } satisfies Changes);
    answers["/api/workspaces/w3/pr"] = () => json(200, shipping({ pushed: true, pr: pr(12, "open", "failing") }));
    answers["/api/workspaces/w4/changes"] = () => json(200, { base: "main", commits: [commit("ddd4444", "Rename the API")], files: [file("src/api.ts", 9, 9)] } satisfies Changes);
    answers["/api/workspaces/w4/pr"] = () => json(200, shipping({ pushed: true, pr: pr(9, "merged", "passing") }));
    answers["/api/workspaces/w5/changes"] = () => json(200, { base: "main", commits: [], files: [] } satisfies Changes);
    answers["/api/workspaces/w5/pr"] = () => json(200, shipping());
  });

  it("puts the cards in lanes by how far their work has gone, counts each, and sums the fleet's work", async () => {
    await show(fleet(fleetOf(), listed()), vi.fn(), {}, true);
    const toggle = screen.getByRole("button", { name: "Shipping" });
    expect(toggle.getAttribute("aria-pressed")).toBe("false");
    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-pressed")).toBe("true");
    expect(localStorage.getItem("foundry.group")).toBe("shipping");
    await waitFor(() => expect(within(lane("Merged")).getByRole("article", { name: "rename api" })).toBeDefined());
    expect(within(lane("Only here")).getByRole("article", { name: "draft login" })).toBeDefined();
    expect(within(lane("Pushed")).getByRole("article", { name: "push docs" })).toBeDefined();
    expect(within(lane("In review")).getByRole("article", { name: "fix footer" })).toBeDefined();
    expect(within(lane("No changes yet")).getByRole("article", { name: "say hi" })).toBeDefined();
    // Another machine's workspace keeps its card, said to be unread from here, and where its branch stands is never asked.
    expect(within(lane("Not read from here")).getByRole("article", { name: "far off" })).toBeDefined();
    expect(reads).not.toContain("/api/workspaces/w6/pr");
    // The lanes of the need view are gone while grouped by shipping.
    expect(screen.queryByRole("region", { name: "Quiet" })).toBeNull();
    const sum = screen.getByText((_, el) => el?.tagName === "P" && /only here/.test(el.textContent ?? ""));
    expect(sum.textContent).toContain("1 only here›1 pushed›1 in review›1 merged");
    expect(sum.textContent).toContain("+69 −13");
    expect(sum.textContent).toContain("4 commits");
    // The pull request reads on the card as anywhere else, failing checks and all.
    expect(within(card("fix footer")).getByText("Checks failing")).toBeDefined();

    fireEvent.click(screen.getByRole("button", { name: "Attention" }));
    expect(screen.getByRole("region", { name: "Working" })).toBeDefined();
    expect(screen.queryByRole("region", { name: "Merged" })).toBeNull();
    expect(localStorage.getItem("foundry.group")).toBe("attention");
  });

  it("opens grouped by shipping for a browser that kept the old ship view, which is no longer a view of its own", async () => {
    localStorage.setItem("foundry.home", "ship");
    await show(fleet(fleetOf(), listed()), vi.fn(), {}, true);
    await waitFor(() => expect(within(lane("Merged")).getByRole("article", { name: "rename api" })).toBeDefined());
    expect(screen.getByRole("button", { name: "Shipping" }).getAttribute("aria-pressed")).toBe("true");
    expectButtonShape(screen.getByRole("button", { name: "Shipping" }));
    expect(screen.queryByRole("radio", { name: "Ship" })).toBeNull();
    expect(within(screen.getByRole("navigation", { name: "Home" })).queryByRole("button", { name: "Ship" })).toBeNull();
  });

  it("offers an idle session the step that takes its work further, under it on its card, and types it into its terminal", async () => {
    await show(fleet(fleetOf(), listed()), vi.fn(), {}, true);
    const draft = card("draft login");
    const step = await within(draft).findByRole("button", { name: "Commit 1 file" });
    expect(within(draft).getByRole("group", { name: "Next for opus" })).toBeDefined();
    fireEvent.click(step);
    const socket = FakeSocket.all.at(-1)!;
    expect(socket.url).toContain("/api/terminals/t1");
    act(() => socket.open());
    expect(new TextDecoder().decode(socket.sent[0] as Uint8Array)).toContain("Commit the uncommitted changes");
    // Failing checks are offered red to the idle session whose pull request has them.
    expect(await within(card("fix footer")).findByRole("button", { name: "Fix failing checks" })).toBeDefined();
    // A working session is not interrupted, and another machine's is not offered anything.
    await waitFor(() => expect(within(card("push docs")).getByText("Document the API")).toBeDefined());
    expect(within(card("push docs")).queryByRole("group", { name: /Next for/ })).toBeNull();
    expect(within(card("far off")).queryByRole("group", { name: /Next for/ })).toBeNull();
  });

  it("offers Archive on the card of a workspace on another machine, and opens its dialog without reading its changes", async () => {
    answers["/api/workspaces/w6/changes"] = () => json(404, { error: "workspace w6 is not a live workspace of this machine" });
    await show(fleet(fleetOf(), listed()), vi.fn(), {}, true);
    const far = card("far off");
    const button = await within(far).findByRole("button", { name: "Archive far off" });
    // Its card asks this machine's daemon, which does not have it; the dialog asks nothing.
    await waitFor(() => expect(readsOf("w6")).toBe(1));
    fireEvent.click(button);
    const dialog = await screen.findByRole("dialog", { name: "Archive far off?" });
    expect(within(dialog).getByRole("region", { name: "Kept on far-off" }).textContent).toContain("laptop lists any work that would be lost");
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(readsOf("w6")).toBe(1);
  });

  it("moves a card to its new lane when one of its terminals changes state, and lights it up once there", async () => {
    localStorage.setItem("foundry.group", "shipping");
    const { update } = await show(fleet(fleetOf("working"), listed()), vi.fn(), {}, true);
    await waitFor(() => expect(within(lane("Only here")).getByRole("article", { name: "draft login" })).toBeDefined());
    answers["/api/workspaces/w1/changes"] = () => json(200, { base: "main", commits: [commit("aaa1111", "Draft the login form")], files: [file("src/login.ts", 40, 2)] } satisfies Changes);
    answers["/api/workspaces/w1/pr"] = () => json(200, shipping({ pushed: true }));
    update(fleet(fleetOf("idle"), listed()));
    await waitFor(() => expect(within(lane("Pushed")).getByRole("article", { name: "draft login" })).toBeDefined());
    expect(within(lane("Pushed")).getByRole("article", { name: "draft login" }).dataset.arrived).toBe("true");
    expect(within(lane("Pushed")).getByRole("article", { name: "push docs" }).dataset.arrived).toBeUndefined();
    expect(reads.filter((u) => u === "/api/workspaces/w3/pr").length).toBe(1);
  });
});

describe("The home's head", () => {
  it("stays the same over the board, the timeline and the machines, and keeps a task being typed while the view changes", async () => {
    const { router } = await show(fleet(records(), settings()));
    const task = () => screen.getByRole("textbox", { name: "New task" }) as HTMLTextAreaElement;
    fireEvent.change(task(), { target: { value: "make the footer say the version" } });

    fireEvent.click(screen.getByRole("radio", { name: "Machines" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/machines"));
    expect(screen.getByRole("region", { name: "Machines" })).toBeDefined();
    // The same head: the task bar with its jump, the line of what needs you, the counts and the switch.
    expect(task().value).toBe("make the footer say the version");
    expect(screen.getByRole("button", { name: "Jump to a session or shell" })).toBeDefined();
    expect(screen.getByText("1 needs you · 1 working · 1 quiet")).toBeDefined();
    expect(screen.getByText("3 sessions · 1 shell · 3 workspaces · 1 machine")).toBeDefined();
    expect(screen.getByRole("radio", { name: "Machines" }).getAttribute("aria-checked")).toBe("true");
    expect(within(screen.getByRole("navigation", { name: "Home" })).getByRole("button", { name: "Machines" }).getAttribute("aria-current")).toBe("page");

    fireEvent.click(screen.getByRole("radio", { name: "Timeline" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/"));
    expect(await screen.findByRole("region", { name: "Timeline" })).toBeDefined();
    expect(screen.queryByRole("region", { name: "Machines" })).toBeNull();
    expect(task().value).toBe("make the footer say the version");
    expect(localStorage.getItem("foundry.home")).toBe("timeline");
  });

  it("says when a machine is offline in the counts", async () => {
    const two = settings({ machines: [{ id: ME, name: "studio", address: "" }, { id: OTHER, name: "laptop", address: "" }] });
    await show(fleet([...records(), sample(OTHER, 1), workspace(OTHER, "w9", "far off"), terminal(OTHER, "t9", "w9", "unreachable")], two));
    expect(screen.getByText(/2 machines · 1 offline$/)).toBeDefined();
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

    await waitFor(() => expect(created).toHaveBeenCalledWith("s9", null));
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

/** Project p1's believed status: run 162 builds in w1, run 170 is held up in w2. */
describe("Board and the factory", () => {
  const factoryRecords = (): AnyRecord[] => [
    sample(ME, 1),
    workspace(ME, "w1", "fac 162"),
    terminal(ME, "t1", "w1"),
    session(ME, "s1", "w1", "t1", "stuck"),
    workspace(ME, "w2", "fac 170"),
    terminal(ME, "t2", "w2"),
    session(ME, "s2", "w2", "t2", "finished"),
    onCommit(ME, "w3", "far away"),
  ];
  beforeEach(() => {
    answers["/api/workspaces/w1/changes"] = () => json(200, { base: "main", commits: [], files: [] } satisfies Changes);
  });

  it("marks a factory run's card and links it to its issue", async () => {
    await show(withFactory(fleet(factoryRecords(), settings()), factoryStatuses()));
    const mark = within(card("fac 162")).getByRole("link", { name: "factory #162" });
    expect(mark.getAttribute("href")).toBe("https://github.com/original-solutions/foundry/issues/162");
    expect(within(card("far away")).queryByRole("link", { name: /^factory/ })).toBeNull();
  });

  it("marks a run whose id names no issue as plain text, with no link", async () => {
    await show(withFactory(fleet(factoryRecords(), settings()), factoryStatus("p1", { "not-a-run": ["w1"] })));
    const run = card("fac 162");
    expect(within(run).getByText("factory").tagName).toBe("SPAN");
    expect(within(run).queryByRole("link", { name: /^factory/ })).toBeNull();
  });

  it("keeps a factory session P1 marks stuck under working, quiet 10+ min, and a held-up run's card under needs you", async () => {
    await show(withFactory(fleet(factoryRecords(), settings()), factoryStatuses()));
    const working = screen.getByRole("region", { name: "Working" });
    expect(within(within(working).getByRole("article", { name: "fac 162" })).getByText("Quiet 10+ min")).toBeDefined();
    expect(within(screen.getByRole("region", { name: "Needs you" })).getByRole("article", { name: "fac 170" })).toBeDefined();
  });

  it("moves a needs-info run's card under needs you too", async () => {
    await show(withFactory(fleet(factoryRecords(), settings()), factoryStatuses("needs_info")));
    expect(within(screen.getByRole("region", { name: "Needs you" })).getByRole("article", { name: "fac 170" })).toBeDefined();
  });

  it("marks a factory run's line and shows a commit's line by its commit in the list too", async () => {
    localStorage.setItem("foundry.layout", "list");
    await show(withFactory(fleet(factoryRecords(), settings()), factoryStatuses()));
    expect(within(card("fac 162")).getByRole("link", { name: "factory #162" }).getAttribute("href")).toBe("https://github.com/original-solutions/foundry/issues/162");
    expect(within(card("far away")).queryByRole("link", { name: /^factory/ })).toBeNull();
    expect(within(card("far away")).getByText("demo · fedcba9")).toBeDefined();
  });

  it("shows a workspace with no branch by its commit", async () => {
    await show(fleet(factoryRecords(), settings()));
    expect(within(card("far away")).getByText("demo · fedcba9")).toBeDefined();
  });
});
