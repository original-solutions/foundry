import { describe, expect, it } from "vitest";
import type { Changes } from "./api";
import { glance, lanes, topFiles } from "./lanes";
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
