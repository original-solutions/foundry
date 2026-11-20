import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, RouterProvider } from "@tanstack/react-router";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createContext, useContext } from "react";
import type { AnyRecord, Changes, SessionState, Shipping } from "./api";
import { fleet, type MachineView } from "./fleet";
import { Ship } from "./Ship";
import { ME, OTHER, sample, session, settings, terminal, workspace } from "./test-fixtures";
import { FakeSocket, json } from "./test-utils";

const file = (path: string, added: number, removed: number, uncommitted = false) => ({ path, from: null, status: "modified" as const, added, removed, uncommitted, patch: "" });
const commit = (hash: string, subject: string) => ({ hash, subject });
const shipping = (extra: Partial<Shipping> = {}): Shipping => ({ branch: "b", pushed: false, unpushed: 0, pr: null, unreachable: false, ...extra });

const records = (draft: SessionState = "idle"): AnyRecord[] => [
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
  workspace(OTHER, "w6", "far away"),
  terminal(OTHER, "t6", "w6"),
];

const answers: Record<string, () => Response> = {};
let reads: string[] = [];
const readsOf = (path: string) => reads.filter((u) => u === path).length;

beforeEach(() => {
  reads = [];
  FakeSocket.all = [];
  vi.stubGlobal("WebSocket", FakeSocket);
  answers["/api/machine"] = () => json(200, { id: ME });
  answers["/api/workspaces/w1/changes"] = () => json(200, { base: "main", commits: [commit("aaa1111", "Draft the login form")], files: [file("src/login.ts", 40, 2), file("README.md", 3, 0, true)] } satisfies Changes);
  answers["/api/workspaces/w1/pr"] = () => json(200, shipping({ unpushed: 1 }));
  answers["/api/workspaces/w2/changes"] = () => json(200, { base: "main", commits: [commit("bbb2222", "Document the API")], files: [file("docs/api.md", 12, 1)] } satisfies Changes);
  answers["/api/workspaces/w2/pr"] = () => json(200, shipping({ pushed: true }));
  answers["/api/workspaces/w3/changes"] = () => json(200, { base: "main", commits: [commit("ccc3333", "Show the version in the footer")], files: [file("src/footer.tsx", 5, 1)] } satisfies Changes);
  answers["/api/workspaces/w3/pr"] = () =>
    json(200, shipping({ pushed: true, pr: { number: 12, title: "Show the version", url: "https://github.com/o/r/pull/12", state: "open", draft: false, checks: "failing", review: "none" } }));
  answers["/api/workspaces/w4/changes"] = () => json(200, { base: "main", commits: [commit("ddd4444", "Rename the API")], files: [file("src/api.ts", 9, 9)] } satisfies Changes);
  answers["/api/workspaces/w4/pr"] = () =>
    json(200, shipping({ pushed: true, pr: { number: 9, title: "Rename the API", url: "https://github.com/o/r/pull/9", state: "merged", draft: false, checks: "passing", review: "approved" } }));
  answers["/api/workspaces/w5/changes"] = () => json(200, { base: "main", commits: [], files: [] } satisfies Changes);
  answers["/api/workspaces/w5/pr"] = () => json(200, shipping());
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      reads.push(url);
      const answer = answers[url];
      return answer ? answer() : json(404, { error: "not found" });
    }),
  );
});

afterEach(() => {
  vi.unstubAllGlobals();
});

const Fleet = createContext<MachineView[]>([]);

async function show(machines: MachineView[], onChanges = vi.fn()) {
  function Shown() {
    return <Ship machines={useContext(Fleet)} onChanges={onChanges} />;
  }
  const root = createRootRoute({ component: Shown });
  const router = createRouter({
    routeTree: root.addChildren([createRoute({ getParentRoute: () => root, path: "/terminals/$terminalId" })]),
    history: createMemoryHistory({ initialEntries: ["/"] }),
  });
  await router.load();
  const client = new QueryClient();
  const tree = (list: MachineView[]) => (
    <Fleet.Provider value={list}>
      <QueryClientProvider client={client}>
        <RouterProvider router={router} />
      </QueryClientProvider>
    </Fleet.Provider>
  );
  const view = render(tree(machines));
  return { router, update: (next: MachineView[]) => view.rerender(tree(next)) };
}

const column = (name: string) => screen.getByRole("region", { name });

describe("Ship", () => {
  it("puts each workspace in the column of how far its work has gone, and counts each column", async () => {
    await show(fleet(records(), settings()));
    await waitFor(() => expect(within(column("Merged")).getByRole("article", { name: "rename api" })).toBeDefined());
    expect(within(column("Only here")).getByRole("article", { name: "draft login" })).toBeDefined();
    expect(within(column("Pushed")).getByRole("article", { name: "push docs" })).toBeDefined();
    expect(within(column("In review")).getByRole("article", { name: "fix footer" })).toBeDefined();
    for (const name of ["Only here", "Pushed", "In review", "Merged"]) expect(within(column(name)).getByText("1", { selector: "[data-count]" })).toBeDefined();
    // What a card shows: its numbers, its newest commit, and what is still only here.
    const draft = within(column("Only here")).getByRole("article", { name: "draft login" });
    expect(within(draft).getByText("+43")).toBeDefined();
    expect(within(draft).getByText("Draft the login form")).toBeDefined();
    expect(within(draft).getByText("1 uncommitted")).toBeDefined();
    // The pull request line reads as everywhere else, failing checks and all.
    expect(within(column("In review")).getByRole("link", { name: /PR #12/ }).getAttribute("href")).toBe("https://github.com/o/r/pull/12");
    expect(within(column("In review")).getByText("Checks failing")).toBeDefined();
  });

  it("sums the fleet's work above the columns, and names what changed nothing", async () => {
    await show(fleet(records(), settings()));
    const sum = (await screen.findByText(/4 workspaces with work/)).closest("p")!;
    expect(sum.textContent).toContain("+69 −13");
    expect(sum.textContent).toContain("4 commits");
    const none = screen.getByRole("region", { name: "Nothing yet" });
    expect(within(none).getByRole("link", { name: /say hi/ })).toBeDefined();
  });

  it("reads only this machine's workspaces; another machine's are named as not read from here", async () => {
    await show(fleet(records(), settings()));
    await screen.findByText(/4 workspaces with work/);
    expect(reads.some((u) => u.includes("/w6/"))).toBe(false);
    expect(screen.getByText(/far away/).closest("p")?.textContent).toMatch(/not read from here/);
  });

  it("offers an idle session the step that takes its work further, and types it into its terminal", async () => {
    await show(fleet(records(), settings()));
    const draft = await waitFor(() => within(column("Only here")).getByRole("article", { name: "draft login" }));
    const step = await within(draft).findByRole("button", { name: "Commit 1 file" });
    fireEvent.click(step);
    const socket = FakeSocket.all.at(-1)!;
    expect(socket.url).toContain("/api/terminals/t1");
    act(() => socket.open());
    expect(new TextDecoder().decode(socket.sent[0] as Uint8Array)).toContain("Commit the uncommitted changes");
    // A working session is not interrupted: its card has no steps.
    const pushed = within(column("Pushed")).getByRole("article", { name: "push docs" });
    expect(within(pushed).queryByRole("button", { name: /Open a PR/ })).toBeNull();
  });

  it("reads a workspace again when one of its terminals changes state, and moves its card", async () => {
    const { update } = await show(fleet(records("working"), settings()));
    await waitFor(() => expect(readsOf("/api/workspaces/w1/pr")).toBe(1));
    answers["/api/workspaces/w1/changes"] = () => json(200, { base: "main", commits: [commit("aaa1111", "Draft the login form")], files: [file("src/login.ts", 40, 2)] } satisfies Changes);
    answers["/api/workspaces/w1/pr"] = () => json(200, shipping({ pushed: true }));
    update(fleet(records("idle"), settings()));
    await waitFor(() => expect(within(column("Pushed")).getByRole("article", { name: "draft login" })).toBeDefined());
    // It lights up once in its new column; the cards that did not move do not.
    expect(within(column("Pushed")).getByRole("article", { name: "draft login" }).dataset.arrived).toBe("true");
    expect(within(column("Pushed")).getByRole("article", { name: "push docs" }).dataset.arrived).toBeUndefined();
    expect(readsOf("/api/workspaces/w1/pr")).toBe(2);
    expect(readsOf("/api/workspaces/w3/pr")).toBe(1);
  });
});
