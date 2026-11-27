import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { AnyRecord, Briefs, FactoryStatus, Mark, SessionState } from "./api";
import { AWAY_KEY, AWAY_MS } from "./absence";
import { Away } from "./Away";
import { fleet } from "./fleet";
import { factoryStatus, ME, sample, session, settings, terminal, workspace } from "./test-fixtures";
import { json } from "./test-utils";

const MIN = 60_000;
let clock = 0;
let trails: Record<string, Mark[]> = {};
let reads: string[] = [];

/** The fleet as the page holds it: s1 and s2 in `fix login`, s3 in `tidy readme`, and a shell. */
function fleetAt(states: { s1?: SessionState; s2?: SessionState; s3?: SessionState } = {}, versions: { s1?: number; s2?: number; s3?: number } = {}, extra: AnyRecord[] = []): AnyRecord[] {
  return [
    sample(ME, 1),
    workspace(ME, "w1", "fix login"),
    terminal(ME, "t1", "w1"),
    session(ME, "s1", "w1", "t1", states.s1 ?? "idle", { version: versions.s1 ?? 1 }),
    terminal(ME, "t2", "w1"),
    session(ME, "s2", "w1", "t2", states.s2 ?? "idle", { model: "sonnet", version: versions.s2 ?? 1 }),
    workspace(ME, "w2", "tidy readme"),
    terminal(ME, "t3", "w2"),
    session(ME, "s3", "w2", "t3", states.s3 ?? "idle", { version: versions.s3 ?? 1 }),
    terminal(ME, "t9", "w2"),
    ...extra,
  ];
}

const briefs: Briefs = {
  s1: { title: "add the login form", now: null, said: "The form is in.\nTests pass." },
  s2: { title: "tidy the docs", now: { kind: "ask", text: "Delete old.md?" } },
  s3: { title: "hi", now: null },
};

const at = (minutes: number) => clock + minutes * MIN;
const mark = (kind: Mark["kind"], minutes: number, extra: Partial<Mark> = {}): Mark => ({ kind, text: "", at: at(minutes), ...extra });

let visibility: DocumentVisibilityState = "visible";
function setVisibility(next: DocumentVisibilityState) {
  visibility = next;
  act(() => {
    document.dispatchEvent(new Event("visibilitychange"));
  });
}
/** Hides the page at minute `from` and shows it again at minute `to`. */
function away(from: number, to: number) {
  vi.spyOn(Date, "now").mockReturnValue(at(from));
  setVisibility("hidden");
  vi.spyOn(Date, "now").mockReturnValue(at(to));
  setVisibility("visible");
}

beforeEach(() => {
  clock = 1_800_000_000_000;
  vi.spyOn(Date, "now").mockReturnValue(clock);
  Object.defineProperty(document, "visibilityState", { configurable: true, get: () => visibility });
  visibility = "visible";
  localStorage.clear();
  reads = [];
  trails = {
    s1: [mark("prompt", -30, { text: "add the login form" }), mark("done", -20), mark("prompt", 3, { text: "now the tests" }), mark("tool", 4, { tool: "Bash", text: "$ npm test" }), mark("tool", 5, { tool: "Edit", text: "Edit a.ts" }), mark("tool", 6, { tool: "Bash", text: "$ npm test" }), mark("done", 8), mark("ask", 10, { text: "Claude is waiting for your input" })],
    s2: [mark("prompt", -40, { text: "tidy the docs" }), mark("ask", 9, { text: "Delete old.md?" })],
    s3: [mark("prompt", -50, { text: "hi" }), mark("done", -49)],
  };
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      reads.push(url);
      const id = /\/api\/sessions\/(\w+)\/trail/.exec(url)?.[1] ?? "";
      return trails[id] ? json(200, { steps: trails[id] }) : json(404, { error: "no session here" });
    }),
  );
});

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  localStorage.clear();
});

function view(records: AnyRecord[], onOpen = vi.fn(), statuses?: Record<string, FactoryStatus>) {
  const client = new QueryClient();
  const ui = (r: AnyRecord[]) => (
    <QueryClientProvider client={client}>
      <Away records={r} synced machines={fleet(r, settings(), briefs)} traced={new Set(["s1", "s2", "s3"])} onOpen={onOpen} statuses={statuses} />
    </QueryClientProvider>
  );
  const rendered = render(ui(records));
  return { onOpen, client, unmount: rendered.unmount, move: (r: AnyRecord[]) => rendered.rerender(ui(r)) };
}

const panel = () => screen.getByRole("complementary", { name: "While you were away" });

describe("Away", () => {
  it("says, on coming back, what each session did while the page was hidden, what needs you first", async () => {
    const { move } = view(fleetAt());
    vi.spyOn(Date, "now").mockReturnValue(at(0));
    setVisibility("hidden");
    // While away: s1 took a turn and rests, s2 asked a question; s3 did nothing.
    move(fleetAt({ s1: "idle", s2: "waiting_for_you" }, { s1: 3, s2: 2 }));
    vi.spyOn(Date, "now").mockReturnValue(at(12));
    setVisibility("visible");

    // The reminder Claude Code sends after a turn ends is not a question.
    await waitFor(() => expect(within(panel()).getByText("· 1 turn · 2 commands · 1 edit")).toBeDefined());
    expect(within(panel()).getByText("12m")).toBeDefined();
    const items = within(panel()).getAllByRole("button", { name: /^Open / });
    expect(items.map((b) => b.getAttribute("aria-label"))).toEqual(["Open sonnet in fix login: Needs you", "Open opus in fix login: Ready"]);
    // The question it waits on, and what Claude said at the end of the turn.
    expect(within(items[0]).getByText("Delete old.md?")).toBeDefined();
    expect(within(items[1]).getByText(/The form is in\./)).toBeDefined();
    expect(within(panel()).getByText("2 sessions · 1 turn · 3 tool calls · 1 needs you")).toBeDefined();
    // Only the sessions that moved are read; s3 did not.
    expect(reads.some((url) => url.includes("/s3/"))).toBe(false);
  });

  it("says a live factory run's session that went stuck is quiet, and counts it as needing no one", async () => {
    const { move } = view(fleetAt(), vi.fn(), factoryStatus("p1", { "original-solutions__foundry__162__1": ["w1"] }));
    vi.spyOn(Date, "now").mockReturnValue(at(0));
    setVisibility("hidden");
    move(fleetAt({ s1: "idle", s2: "stuck" }, { s1: 3, s2: 2 }));
    vi.spyOn(Date, "now").mockReturnValue(at(12));
    setVisibility("visible");

    const line = await within(panel()).findByRole("button", { name: "Open sonnet in fix login: Quiet 10+ min" });
    expect(line.className).not.toContain("bg-amber");
    await waitFor(() => expect(within(panel()).getByText("2 sessions · 1 turn · 3 tool calls")).toBeDefined());
  });

  it("leaves out a live factory run's session that was stuck already, but tells of a held-up run's", async () => {
    const before = fleetAt({ s2: "stuck", s3: "stuck" });
    const statuses = factoryStatus("p1", { "original-solutions__foundry__162__1": ["w1"] }, { "original-solutions__foundry__170__1": { kind: "stuck", workspaces: ["w2"] } });
    const { move } = view(before, vi.fn(), statuses);
    vi.spyOn(Date, "now").mockReturnValue(at(0));
    setVisibility("hidden");
    move(fleetAt({ s1: "idle", s2: "stuck", s3: "stuck" }, { s1: 3 }));
    vi.spyOn(Date, "now").mockReturnValue(at(12));
    setVisibility("visible");

    await within(panel()).findByRole("button", { name: /^Open opus in fix login/ });
    const items = within(panel()).getAllByRole("button", { name: /^Open / });
    expect(items.map((b) => b.getAttribute("aria-label"))).toEqual(["Open opus in tidy readme: Stuck", "Open opus in fix login: Ready"]);
  });

  it("puts a session that needs you above a live factory run's session, whatever their states", async () => {
    const { move } = view(fleetAt(), vi.fn(), factoryStatus("p1", { "original-solutions__foundry__162__1": ["w1"] }));
    vi.spyOn(Date, "now").mockReturnValue(at(0));
    setVisibility("hidden");
    move(fleetAt({ s2: "waiting_for_you", s3: "failed" }, { s2: 2, s3: 2 }));
    vi.spyOn(Date, "now").mockReturnValue(at(12));
    setVisibility("visible");

    await within(panel()).findByRole("button", { name: /^Open opus in tidy readme/ });
    const items = within(panel()).getAllByRole("button", { name: /^Open / });
    expect(items.map((b) => b.getAttribute("aria-label")?.split(":")[0])).toEqual(["Open opus in tidy readme", "Open sonnet in fix login"]);
  });

  it("stays out of the way after a short absence, or a long one in which nothing happened", () => {
    const { move } = view(fleetAt());
    away(0, 1);
    move(fleetAt({ s1: "working" }, { s1: 2 }));
    away(1, 1.5);
    expect(screen.queryByRole("complementary")).toBeNull();

    away(2, 60);
    expect(screen.queryByRole("complementary")).toBeNull();
  });

  it("opens a session's terminal from its line and takes the line off; Dismiss closes it all", async () => {
    const { onOpen, move } = view(fleetAt());
    vi.spyOn(Date, "now").mockReturnValue(at(0));
    setVisibility("hidden");
    move(fleetAt({ s1: "idle", s2: "waiting_for_you" }, { s1: 3, s2: 2 }));
    vi.spyOn(Date, "now").mockReturnValue(at(12));
    setVisibility("visible");

    fireEvent.click(await within(panel()).findByRole("button", { name: /^Open sonnet/ }));
    expect(onOpen).toHaveBeenCalledWith("t2");
    expect(within(panel()).queryByRole("button", { name: /^Open sonnet/ })).toBeNull();

    fireEvent.keyDown(within(panel()).getByRole("button", { name: /^Open opus/ }), { key: "Escape" });
    expect(screen.queryByRole("complementary")).toBeNull();
  });

  it("closes from its Dismiss button", async () => {
    const { move } = view(fleetAt());
    vi.spyOn(Date, "now").mockReturnValue(at(0));
    setVisibility("hidden");
    move(fleetAt({ s1: "idle" }, { s1: 3 }));
    vi.spyOn(Date, "now").mockReturnValue(at(12));
    setVisibility("visible");
    fireEvent.click(within(panel()).getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByRole("complementary")).toBeNull();
  });

  it("counts a session started while away as new", async () => {
    const { move } = view(fleetAt());
    vi.spyOn(Date, "now").mockReturnValue(at(0));
    setVisibility("hidden");
    trails.s4 = [mark("prompt", 2, { text: "write docs" })];
    move(fleetAt({}, {}, [terminal(ME, "t4", "w2"), session(ME, "s4", "w2", "t4", "working")]));
    vi.spyOn(Date, "now").mockReturnValue(at(10));
    setVisibility("visible");
    expect(within(panel()).getByRole("button", { name: "Open opus 2 in tidy readme: Working" }).textContent).toContain("New");
  });

  it("picks up from when the tab was last hidden, after the page was closed and opened again", async () => {
    vi.spyOn(Date, "now").mockReturnValue(at(0));
    const first = view(fleetAt());
    setVisibility("hidden");
    expect(JSON.parse(localStorage.getItem(AWAY_KEY) ?? "{}").at).toBe(at(0));
    first.unmount();

    vi.spyOn(Date, "now").mockReturnValue(at(0) + AWAY_MS + MIN);
    visibility = "visible";
    view(fleetAt({ s1: "idle" }, { s1: 3 }));

    await waitFor(() => expect(within(panel()).getByRole("button", { name: /^Open opus in fix login/ })).toBeDefined());
    expect(localStorage.getItem(AWAY_KEY)).toBeNull();
  });
});
