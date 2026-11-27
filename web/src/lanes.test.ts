import { describe, expect, it } from "vitest";
import type { Changes, FactoryStatus, HeldUp } from "./api";
import { factoryRuns, glance, lanes, QUIET, topFiles } from "./lanes";
import { fleet } from "./fleet";
import { ME, OTHER, sample, session, settings, terminal, workspace } from "./test-fixtures";

const machines = fleet(
  [
    sample(ME, 1),
    sample(OTHER, 1),
    workspace(ME, "w1", "quiet one"),
    terminal(ME, "t1", "w1"),
    session(ME, "s1", "w1", "t1", "idle"),
    workspace(ME, "w2", "busy one"),
    terminal(ME, "t2", "w2"),
    session(ME, "s2", "w2", "t2", "working"),
    workspace(OTHER, "w3", "asks"),
    terminal(OTHER, "t3", "w3"),
    session(OTHER, "s3", "w3", "t3", "working"),
    terminal(OTHER, "t4", "w3"),
    session(OTHER, "s4", "w3", "t4", "waiting_for_you"),
    workspace(ME, "w4", "setup broke"),
    terminal(ME, "t5", "w4", "exited", { exit_code: 2 }),
    workspace(ME, "w5", "empty"),
  ],
  settings({ machines: [] }),
);

describe("lanes", () => {
  it("sorts workspaces by their most urgent terminal: needs you, then working, then quiet", () => {
    const result = lanes(machines);
    expect(result.map((l) => [l.lane, l.cards.map((c) => c.workspace.name)])).toEqual([
      ["needs", ["asks", "setup broke"]],
      ["working", ["busy one"]],
      ["quiet", ["quiet one", "empty"]],
    ]);
  });

  it("gives each card its machine, its terminals most urgent first and the one to open", () => {
    const asks = lanes(machines)[0].cards[0];
    expect(asks.machine.id).toBe(OTHER);
    expect(asks.rows.map((r) => r.glyph)).toEqual(["waiting", "working"]);
    expect(asks.lead?.terminal).toBe("t4");
    expect(lanes(machines)[2].cards[1].lead).toBeUndefined();
  });

  it("leaves out a lane with nothing in it", () => {
    const calm = fleet([sample(ME, 1), workspace(ME, "w1", "a"), terminal(ME, "t1", "w1"), session(ME, "s1", "w1", "t1", "idle")], settings());
    expect(lanes(calm).map((l) => l.lane)).toEqual(["quiet"]);
  });
});

describe("glance", () => {
  it("says first how many need you, then how many work and rest", () => {
    expect(glance(lanes(machines))).toBe("2 need you · 1 working · 2 quiet");
  });

  it("says plainly when nothing needs you, and leaves out what has nothing in it", () => {
    const calm = fleet([sample(ME, 1), workspace(ME, "w1", "a"), terminal(ME, "t1", "w1"), session(ME, "s1", "w1", "t1", "idle")], settings());
    expect(glance(lanes(calm))).toBe("Nothing needs you · 1 quiet");
    const one = fleet([sample(ME, 1), workspace(ME, "w1", "a"), terminal(ME, "t1", "w1"), session(ME, "s1", "w1", "t1", "waiting_for_you")], settings());
    expect(glance(lanes(one))).toBe("1 needs you");
    expect(glance([])).toBe("Nothing needs you");
  });
});

describe("topFiles", () => {
  const file = (path: string, added: number | null, removed: number | null) => ({
    path,
    from: null,
    status: "modified" as const,
    added,
    removed,
    uncommitted: false,
    patch: null,
  });
  it("names the files that changed most, and how many more there are", () => {
    const changes: Changes = {
      base: "main",
      commits: [],
      files: [file("a.ts", 1, 0), file("src/big.ts", 40, 2), file("bin.png", null, null), file("docs/mid.md", 5, 5)],
    };
    expect(topFiles(changes, 2)).toEqual({ names: ["src/big.ts", "docs/mid.md"], more: 2 });
  });
});

/** A believed status of project `project` with live runs and held-up runs, each naming its workspaces. */
function factory(project: string, live: Record<string, string[]>, held: Record<string, { kind: HeldUp["kind"]; workspaces: string[] }> = {}): Record<string, FactoryStatus> {
  return {
    [project]: {
      project,
      pass: 1,
      holds: true,
      generation: 1,
      not_holding: null,
      idle: null,
      owner_ignored: false,
      ready: [],
      feedback_waiting: [],
      runs: Object.entries(live).map(([run, workspaces]) => ({ run, title: "t", step: "building", machine: ME, gate_retries_used: 0, minutes_used: 3, workspaces, sessions: [] })),
      held_up: Object.entries(held).map(([run, h]) => ({ run, kind: h.kind, reason: "why", workspaces: h.workspaces })),
      gate_infra: [],
      finished: [],
      foreign: [],
    },
  };
}

describe("lanes of factory runs", () => {
  const records = [
    sample(ME, 1),
    workspace(ME, "w1", "fac/162-1"),
    terminal(ME, "t1", "w1"),
    session(ME, "s1", "w1", "t1", "stuck"),
    workspace(ME, "w2", "fac/170-1"),
    terminal(ME, "t2", "w2"),
    session(ME, "s2", "w2", "t2", "finished"),
    workspace(ME, "w3", "by hand"),
    terminal(ME, "t3", "w3"),
    session(ME, "s3", "w3", "t3", "stuck"),
  ];
  const board = fleet(records, settings({ machines: [] }));
  const named = (b: ReturnType<typeof lanes>) => b.map((l) => [l.lane, l.cards.map((c) => c.workspace.name)]);

  it("marks a workspace a believed status names with its run and its issue", () => {
    const statuses = factory("p1", { "original-solutions__foundry__162__1": ["w1"] });
    const of = factoryRuns(statuses);
    const [w1, , w3] = board[0].workspaces;
    expect(of(w1)).toEqual({ run: "original-solutions__foundry__162__1", issue: 162, url: "https://github.com/original-solutions/foundry/issues/162" });
    expect(of(w3)).toBeUndefined();
    const card = lanes(board, statuses)
      .flatMap((l) => l.cards)
      .find((c) => c.workspace.id === "w1");
    expect(card?.factory?.issue).toBe(162);
  });

  it("marks nothing for a status of another project that names the same workspace id", () => {
    const of = factoryRuns(factory("p2", { "o__r__1__1": ["w1"] }));
    expect(of(board[0].workspaces[0])).toBeUndefined();
  });

  it("keeps a factory session P1 marks stuck under working, shown as Quiet 10+ min, while a session by hand that is stuck needs you", () => {
    const result = lanes(board, factory("p1", { "o__r__162__1": ["w1"] }));
    expect(named(result)).toEqual([
      ["needs", ["by hand"]],
      ["working", ["fac/162-1"]],
      ["quiet", ["fac/170-1"]],
    ]);
    const quiet = result[1].cards[0];
    expect(quiet.rows.map((r) => r.label)).toEqual([QUIET]);
    expect(QUIET).toBe("Quiet 10+ min");
    expect(result[0].cards[0].rows.map((r) => r.label)).toEqual(["Stuck"]);
  });

  it("moves a run's card to needs you when its run is stuck or needs info, and only then", () => {
    const statuses = factory("p1", { "o__r__162__1": ["w1"] }, { "o__r__170__1": { kind: "stuck", workspaces: ["w2"] } });
    expect(named(lanes(board, statuses))).toEqual([
      ["needs", ["by hand", "fac/170-1"]],
      ["working", ["fac/162-1"]],
    ]);
    const info = factory("p1", {}, { "o__r__170__1": { kind: "needs_info", workspaces: ["w2"] } });
    // With no live run, w1's stuck session is P1's to show, so it needs you as any other would.
    expect(named(lanes(board, info))).toEqual([["needs", ["fac/162-1", "by hand", "fac/170-1"]]]);
  });

  it("leaves a held-up run's stuck session saying Stuck, on its card under needs you", () => {
    const result = lanes(board, factory("p1", {}, { "o__r__162__1": { kind: "stuck", workspaces: ["w1"] } }));
    const card = result[0].cards.find((c) => c.workspace.id === "w1");
    expect(result[0].lane).toBe("needs");
    expect(card?.rows.map((r) => r.label)).toEqual(["Stuck"]);
  });

  it("keeps a live run's session that waits for an answer out of needs you", () => {
    const asks = fleet([sample(ME, 1), workspace(ME, "w1", "fac/162-1"), terminal(ME, "t1", "w1"), session(ME, "s1", "w1", "t1", "waiting_for_you")], settings({ machines: [] }));
    expect(named(lanes(asks, factory("p1", { "o__r__162__1": ["w1"] })))).toEqual([["working", ["fac/162-1"]]]);
  });

  it("reads statuses from an older peer whose held-up runs name no workspaces", () => {
    const statuses = factory("p1", {});
    statuses.p1.held_up = [{ run: "o__r__170__1", kind: "stuck", reason: "why" } as HeldUp];
    expect(named(lanes(board, statuses))[0]).toEqual(["needs", ["fac/162-1", "by hand"]]);
  });
});
