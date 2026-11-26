import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { FactoryOverview, FactorySettings, FactoryStatus, FinishedRun, ProjectFactory } from "./api";
import { Factory } from "./Factory";
import { fleet } from "./fleet";
import { ME, OTHER, sample, session, settings, terminal, workspace } from "./test-fixtures";
import { json, mockFetch, renderApp } from "./test-utils";

afterEach(() => vi.unstubAllGlobals());

const RUN = "acme__foundry__12__1";
const fleetSettings: FactorySettings = { paused: false, poll_seconds: 30, feedback_seconds: 300, max_runs: 2, max_gates: 3, model: "opus", effort: "high" };

const shared = settings({
  machines: [
    { id: ME, name: "studio", address: "" },
    { id: OTHER, name: "laptop", address: "" },
  ],
  projects: [{ id: "p1", name: "foundry", repo: "https://***@github.com/acme/foundry.git" }],
});
const machines = fleet(
  [
    sample(ME, 1),
    workspace(ME, "w0", "fac/12-0"),
    workspace(ME, "w1", "fac/12-1"),
    terminal(ME, "t1", "w1", "running"),
    terminal(ME, "t2", "w1", "running"),
    session(ME, "s1", "w1", "t2", "working"),
  ],
  shared,
);

const finished = (n: number): FinishedRun => ({ run: `acme__foundry__${n}__1`, title: `done ${n}`, outcome: "pr-open", pr_end: null, pr_url: null });

function status(extra: Partial<FactoryStatus> = {}): FactoryStatus {
  return {
    project: "p1",
    pass: 3,
    holds: true,
    generation: 7,
    not_holding: null,
    idle: null,
    owner_ignored: false,
    ready: [],
    feedback_waiting: [],
    runs: [],
    held_up: [],
    gate_infra: [],
    finished: [],
    foreign: [],
    ...extra,
  };
}

const holding = status({
  owner_ignored: true,
  ready: [
    { issue: 20, title: "Add search", reason: "next in line" },
    { issue: 21, title: "Fix login", reason: "waits for #20" },
  ],
  feedback_waiting: [{ issue: 12, title: "Rework the form", reason: "review feedback is 4 minutes old", run: RUN }],
  runs: [{ run: RUN, title: "Rework the form", step: "building", machine: ME, gate_retries_used: 2, minutes_used: 17, workspaces: ["w0", "w1"], sessions: ["s1"] }],
  held_up: [{ run: "acme__foundry__9__2", kind: "stuck", reason: "the gate failed 3 times" }],
  finished: [
    { run: "acme__foundry__5__1", title: "Shipped one", outcome: "pr-open", pr_end: "merged", pr_url: "https://github.com/acme/foundry/pull/50" },
    { run: "acme__foundry__6__1", title: "Needed words", outcome: "needs-info", pr_end: null, pr_url: null },
    ...Array.from({ length: 23 }, (_, i) => finished(100 + i)),
  ],
  foreign: [{ issue: 33, title: "Taken elsewhere", state: "claimed" }],
});

function overview(entry: { status?: FactoryStatus | null; factory?: ProjectFactory; writer?: string | null } = {}): FactoryOverview {
  return {
    settings: fleetSettings,
    projects: [
      {
        id: "p1",
        name: "foundry",
        factory: entry.factory ?? { on: true, login: "bot", paused: false },
        writer: entry.writer === undefined ? ME : entry.writer,
        status: entry.status === undefined ? holding : entry.status,
      },
    ],
  };
}

async function show(over: FactoryOverview, statuses?: Record<string, FactoryStatus>, extra: Response[] = []) {
  const calls = mockFetch(json(200, over), ...extra);
  await renderApp(<Factory statuses={statuses} settings={shared} machines={machines} />);
  return { calls, project: await screen.findByRole("region", { name: "foundry" }) };
}

describe("Factory", () => {
  it("shows a holding project's lease, queue, runs and finished runs from a fixed status", async () => {
    const { project } = await show(overview(), { p1: holding });
    const p = within(project);
    expect(screen.getByRole("heading", { name: "Factory" })).toBeDefined();
    expect(screen.getByRole("link", { name: "Board" }).getAttribute("href")).toBe("/");
    expect(p.getByText(/Holds the lease at generation 7, on studio/)).toBeDefined();
    expect(p.getByText("This repo sets factory.owner, which Foundry ignores: the lease decides which machine acts.")).toBeDefined();

    const ready = within(p.getByRole("list", { name: "Ready" })).getAllByRole("listitem");
    expect(ready).toHaveLength(2);
    expect(ready[0].textContent).toContain("#20");
    expect(ready[0].textContent).toContain("Add search");
    expect(ready[0].textContent).toContain("next in line");
    expect(ready[1].textContent).toContain("waits for #20");
    expect(within(ready[0]).getByRole("link", { name: "#20" }).getAttribute("href")).toBe("https://github.com/acme/foundry/issues/20");

    const waiting = within(p.getByRole("list", { name: "Feedback waiting" })).getByRole("listitem");
    expect(waiting.textContent).toContain("review feedback is 4 minutes old");

    const live = within(p.getByRole("list", { name: "Live runs" })).getByRole("listitem");
    expect(live.textContent).toContain("Rework the form");
    expect(live.textContent).toContain("building");
    expect(live.textContent).toContain("studio");
    expect(live.textContent).toContain("attempt 1");
    expect(live.textContent).toContain("gate retries 2");
    expect(live.textContent).toContain("17 min");
    expect(within(live).getByRole("link", { name: "#12" }).getAttribute("href")).toBe("https://github.com/acme/foundry/issues/12");
    expect(within(live).getByRole("link", { name: "fac/12-1" }).getAttribute("href")).toBe("/terminals/t2");
    expect(within(live).getByRole("button", { name: "Stop #12" })).toBeDefined();

    const held = within(p.getByRole("list", { name: "Held up" })).getByRole("listitem");
    expect(held.textContent).toContain("#9");
    expect(held.textContent).toContain("stuck");
    expect(held.textContent).toContain("the gate failed 3 times");
    expect(held.textContent).toContain("any uncommitted work there is lost then");

    expect(within(p.getByRole("list", { name: "Claimed by another factory" })).getByText(/Taken elsewhere/)).toBeDefined();

    const done = within(p.getByRole("list", { name: "Finished" })).getAllByRole("listitem");
    expect(done).toHaveLength(20);
    expect(done[0].textContent).toContain("Shipped one");
    expect(done[0].textContent).toContain("merged");
    expect(within(done[0]).getByRole("link", { name: "PR" }).getAttribute("href")).toBe("https://github.com/acme/foundry/pull/50");
    expect(done[1].textContent).toContain("needs-info");
  });

  it("says a run's workspace is not here yet when no machine holds it", async () => {
    const live = status({ runs: [{ run: RUN, title: "t", step: "s", machine: OTHER, gate_retries_used: 0, minutes_used: 0, workspaces: ["gone"], sessions: [] }] });
    const { project } = await show(overview({ status: live }), { p1: live });
    expect(within(project).getByText("workspace not here yet")).toBeDefined();
  });

  it("shows an off factory, a project with no status, and each reason a holder is idle", async () => {
    const cases: [string, Parameters<typeof overview>[0], Record<string, FactoryStatus> | undefined, RegExp][] = [
      ["off", { factory: { on: false, login: "", paused: false }, status: null }, undefined, /Factory off/],
      ["no status", { status: null }, undefined, /No status yet/],
      ["no token", { status: null }, { p1: status({ holds: false, generation: 0, not_holding: { kind: "no_token", login: "bot" } }) }, /Idle: this coordinator holds no token for bot/],
      ["held by", { status: null }, { p1: status({ holds: false, generation: 4, not_holding: { kind: "held_by", machine: OTHER, generation: 4 } }) }, /Idle here: laptop holds the lease at generation 4/],
      ["idle text", { status: null }, { p1: status({ idle: "the factory is paused" }) }, /Idle: the factory is paused/],
    ];
    for (const [name, entry, statuses, line] of cases) {
      const { project } = await show(overview(entry), statuses);
      expect(within(project).getByText(line), name).toBeDefined();
      document.body.innerHTML = "";
    }
  });

  it("shows an empty state for a fleet with no projects", async () => {
    mockFetch(json(200, { settings: fleetSettings, projects: [] }));
    await renderApp(<Factory statuses={undefined} settings={shared} machines={machines} />);
    expect(await screen.findByText(/No projects yet/)).toBeDefined();
  });

  describe("Stop", () => {
    const live = status({ runs: holding.runs });
    const stops = (calls: { url: string; method: string }[]) => calls.filter((c) => c.method === "POST");

    it("asks before it sends, and sends once only when confirmed", async () => {
      const { calls, project } = await show(overview({ status: live }), { p1: live }, [json(200, { stopped: true, already: false, message: "Stop recorded for #12." })]);
      fireEvent.click(within(project).getByRole("button", { name: "Stop #12" }));
      expect(within(project).getByText(/Stop run #12\?/)).toBeDefined();
      expect(stops(calls)).toHaveLength(0);

      fireEvent.click(within(project).getByRole("button", { name: "Cancel" }));
      expect(stops(calls)).toHaveLength(0);
      expect(within(project).queryByText(/Stop run #12\?/)).toBeNull();

      fireEvent.click(within(project).getByRole("button", { name: "Stop #12" }));
      fireEvent.click(within(project).getByRole("button", { name: "Stop the run" }));
      await waitFor(() => expect(stops(calls)).toHaveLength(1));
      expect(stops(calls)[0]).toMatchObject({ url: `/api/factory/runs/${encodeURIComponent(RUN)}/stop`, method: "POST" });
      expect(await within(project).findByText("Stop recorded for #12.")).toBeDefined();
    });

    it("shows the daemon's refusal", async () => {
      const { project } = await show(overview({ status: live }), { p1: live }, [json(409, { error: "this machine does not hold the lease" })]);
      fireEvent.click(within(project).getByRole("button", { name: "Stop #12" }));
      fireEvent.click(within(project).getByRole("button", { name: "Stop the run" }));
      expect((await within(project).findByRole("alert")).textContent).toContain("this machine does not hold the lease");
    });
  });
});
