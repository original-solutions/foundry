import { describe, expect, it } from "vitest";
import { fleet, withFactory } from "./fleet";
import { needing, upNext } from "./needing";
import { factoryStatus, ME, OTHER, sample, session, settings, terminal, workspace } from "./test-fixtures";

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
    workspace(ME, "w5", "also asks"),
    terminal(ME, "t6", "w5"),
    session(ME, "s6", "w5", "t6", "waiting_for_you"),
  ],
  settings({ machines: [] }),
);

describe("needing", () => {
  it("lists every terminal that needs a person, in the board's order, and nothing that works or rests", () => {
    expect(needing(machines).map((n) => [n.workspace, n.name, n.glyph])).toEqual([
      ["also asks", "opus", "waiting"],
      ["asks", "opus 2", "waiting"],
      ["setup broke", "shell", "failed"],
    ]);
  });
});

describe("needing and the factory", () => {
  const records = [
    sample(ME, 1),
    workspace(ME, "w1", "fac 162"),
    terminal(ME, "t1", "w1"),
    session(ME, "s1", "w1", "t1", "stuck"),
    workspace(ME, "w2", "fac 170"),
    terminal(ME, "t2", "w2"),
    session(ME, "s2", "w2", "t2", "finished"),
  ];

  it("leads to a held-up run's card by its most urgent terminal, though none of its terminals needs you, and skips a live run's", () => {
    const run = withFactory(fleet(records, settings()), factoryStatus("p1", { o__r__162__1: ["w1"] }, { o__r__170__1: { kind: "stuck", workspaces: ["w2"] } }));
    expect(needing(run).map((n) => [n.workspace, n.terminal])).toEqual([["fac 170", "t2"]]);
  });
});

describe("upNext", () => {
  it("from the board, offers the first that needs you and counts them all", () => {
    expect(upNext(machines)).toEqual({ next: expect.objectContaining({ terminal: "t6", workspace: "also asks", now: undefined }), others: 3 });
  });

  it("from one that needs you, offers the one after it and counts the others", () => {
    expect(upNext(machines, "t6")).toEqual({ next: expect.objectContaining({ terminal: "t4" }), others: 2 });
  });

  it("goes round from the last to the first, so pressing on walks them all", () => {
    expect(upNext(machines, "t5")?.next.terminal).toBe("t6");
  });

  it("from a terminal that needs nothing, offers the first that does", () => {
    expect(upNext(machines, "t2")).toEqual({ next: expect.objectContaining({ terminal: "t6" }), others: 3 });
  });

  it("offers nothing when only the open terminal needs you, or nothing does", () => {
    const one = fleet([workspace(ME, "w1", "asks"), terminal(ME, "t1", "w1"), session(ME, "s1", "w1", "t1", "waiting_for_you")], settings());
    expect(upNext(one, "t1")).toBeUndefined();
    const none = fleet([workspace(ME, "w1", "busy"), terminal(ME, "t1", "w1"), session(ME, "s1", "w1", "t1", "working")], settings());
    expect(upNext(none)).toBeUndefined();
  });
});
