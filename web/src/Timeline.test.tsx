import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { AnyRecord, Mark, SessionState } from "./api";
import { fleet } from "./fleet";
import { ME, OTHER, sample, session, settings, terminal, workspace } from "./test-fixtures";
import { RETRY_EVERY } from "./poll";
import { STEPS_EVERY } from "./StepsPanel";
import { json } from "./test-utils";
import { Timeline } from "./Timeline";

const MIN = 60_000;
let now = 0;
const ago = (minutes: number) => now - minutes * MIN;
const m = (kind: Mark["kind"], text: string, minutes: number): Mark => ({ kind, text, at: ago(minutes) });
const tool = (name: string, text: string, minutes: number): Mark => ({ kind: "tool", tool: name, text, at: ago(minutes) });

const records: AnyRecord[] = [
  sample(ME, 1),
  workspace(ME, "w1", "fix login"),
  terminal(ME, "t1", "w1"),
  session(ME, "s1", "w1", "t1", "working"),
  terminal(ME, "t2", "w1"),
  session(ME, "s2", "w1", "t2", "idle", { model: "sonnet" }),
  workspace(ME, "w2", "tidy readme"),
  terminal(ME, "t3", "w2"),
  session(ME, "s3", "w2", "t3", "idle"),
  sample(OTHER, 1),
  workspace(OTHER, "w4", "elsewhere"),
  terminal(OTHER, "t4", "w4"),
  session(OTHER, "s4", "w4", "t4", "idle"),
];

let trails: Record<string, Mark[] | undefined> = {};
let reads: string[] = [];
/** What the daemon answers a trail read with, when not the trail. */
let refuse: Record<string, number> = {};

beforeEach(() => {
  now = Date.now();
  reads = [];
  refuse = {};
  localStorage.clear();
  trails = {
    s1: [m("prompt", "add the login form", 40), tool("Bash", "$ npm test", 39), m("done", "", 35), m("prompt", "now check the session", 3), tool("Edit", "Edit src/login.ts", 2)],
    s2: [m("prompt", "tidy the docs", 150), m("ask", "Delete old.md?", 149), m("done", "", 140)],
    s3: [m("prompt", "hi", 300), m("done", "", 299), m("ask", "Claude is waiting for your input", 20)],
  };
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      reads.push(url);
      const id = /\/api\/sessions\/(\w+)\/trail/.exec(url)?.[1] ?? "";
      const steps = trails[id];
      if (refuse[id]) return json(refuse[id], { error: "daemon hiccup" });
      return steps ? json(200, { steps }) : json(404, { error: "no session here" });
    }),
  );
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

const traced = new Set(["s1", "s2", "s3"]);

function show(onSteps = vi.fn()) {
  render(
    <QueryClientProvider client={new QueryClient()}>
      <Timeline machines={fleet(records, settings())} traced={traced} onSteps={onSteps} />
    </QueryClientProvider>,
  );
  return onSteps;
}

/** `records` with session s1 in `state`. */
const withS1 = (state: SessionState) => records.map((r) => (r.kind === "session" && r.body.id === "s1" ? { ...r, body: { ...r.body, state } } : r));

/** The timeline on a fake clock with s1 in `state`; `move` puts s1 in another state, as a hook would. */
async function watch(state: SessionState) {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  const client = new QueryClient();
  const at = (s: SessionState) => (
    <QueryClientProvider client={client}>
      <Timeline machines={fleet(withS1(s), settings())} traced={traced} onSteps={vi.fn()} />
    </QueryClientProvider>
  );
  const view = render(at(state));
  await waitFor(() => expect(readsOf("s1")).toBe(1));
  return { client, move: (s: SessionState) => view.rerender(at(s)) };
}

const readsOf = (id: string) => reads.filter((url) => url.includes(`/${id}/`)).length;
const wait = (ms: number) => act(() => vi.advanceTimersByTimeAsync(ms));

const lane = (name: RegExp) => screen.getByRole("group", { name });
/** The bars of a lane: one button per turn. */
const barsOf = (name: RegExp) => within(lane(name)).getAllByRole("button", { name: /ago$|running for/ });

describe("Timeline", () => {
  it("draws each session with turns in the last hour as a lane of bars, busy ones first", async () => {
    show();
    await waitFor(() => expect(screen.getAllByRole("group")).toHaveLength(1));
    const bars = barsOf(/opus · fix login/);
    expect(bars.map((b) => b.getAttribute("aria-label"))).toEqual([
      "“add the login form”, 1 command, took 5m 0s, 40m ago",
      "“now check the session”, 1 edit, running for 3m 0s",
    ]);
    // The sonnet session's only turn was 2½ hours ago; the third only a reminder; the peer's are kept on its machine.
    expect(screen.getByText("3 more sessions took no turns in the last hour")).toBeDefined();
    expect(reads.some((url) => url.includes("/s4/"))).toBe(false);
  });

  it("adds up the window: turns, agent time, tool calls and questions", async () => {
    show();
    await waitFor(() => expect(screen.getByText(/2 turns/)).toBeDefined());
    expect(screen.getByText("2 turns · 8m 0s of agent work · 2 tool calls")).toBeDefined();
  });

  it("widens the window to take in older turns", async () => {
    show();
    await waitFor(() => expect(screen.getAllByRole("group")).toHaveLength(1));
    fireEvent.click(screen.getByRole("radio", { name: "4h" }));
    expect(screen.getAllByRole("group")).toHaveLength(2);
    expect(barsOf(/sonnet · fix login/)[0].getAttribute("aria-label")).toBe("“tidy the docs”, 1 question, took 10m 0s, 2h ago");
    expect(screen.getByText(/1 question$/)).toBeDefined();
  });

  it("opens a turn's terminal with its steps", async () => {
    const onSteps = show();
    await waitFor(() => expect(screen.getAllByRole("group")).toHaveLength(1));
    fireEvent.click(barsOf(/opus · fix login/)[0]);
    expect(onSteps).toHaveBeenCalledWith("t1");
  });

  it("says so when no session took a turn in the window", async () => {
    trails = { s1: [], s2: [], s3: [] };
    show();
    await waitFor(() => expect(screen.getByText(/No agent took a turn in the last hour/)).toBeDefined());
    expect(screen.queryAllByRole("group")).toHaveLength(0);
  });

  it("reads a working session's trail every STEPS_EVERY, and one that waits only when its state changes, under the steps column's one entry", async () => {
    const { client, move } = await watch("working");
    await wait(5 * STEPS_EVERY + 100);
    expect(readsOf("s1")).toBe(6);
    move("waiting_for_you");
    await waitFor(() => expect(readsOf("s1")).toBe(7));
    await wait(60_000);
    expect(readsOf("s1")).toBe(7);
    move("idle");
    await waitFor(() => expect(readsOf("s1")).toBe(8));
    expect(client.getQueryCache().findAll({ queryKey: ["trail", "s1"] })).toHaveLength(1);
  });

  it("keeps reading a working session after a failed read, at the back-off, and stops on a 404", async () => {
    refuse = { s1: 500 };
    await watch("working");
    await wait(STEPS_EVERY + 100);
    expect(readsOf("s1")).toBe(1);
    await wait(RETRY_EVERY - STEPS_EVERY);
    expect(readsOf("s1")).toBe(2);
    refuse = { s1: 404 };
    await wait(RETRY_EVERY);
    expect(readsOf("s1")).toBe(3);
    await wait(60_000);
    expect(readsOf("s1")).toBe(3);
  });
});
