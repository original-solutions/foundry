import { describe, expect, it } from "vitest";
import { endOf, ended } from "./ends";
import { fleet } from "./fleet";
import type { Glyph } from "./labels";
import { ME, sample, session, settings, terminal, workspace } from "./test-fixtures";

describe("endOf", () => {
  it("ends what still runs, after asking, and clears what already ended, at once", () => {
    const end: Glyph[] = ["starting", "working", "waiting", "ready", "stuck"];
    const clear: Glyph[] = ["done", "failed"];
    for (const glyph of end) expect(endOf(glyph)).toBe("end");
    for (const glyph of clear) expect(endOf(glyph)).toBe("clear");
  });

  it("offers nothing for a terminal whose machine cannot be reached", () => {
    expect(endOf("away")).toBeUndefined();
  });
});

describe("ended", () => {
  it("lists a workspace's terminals that only keep their final screen, sessions first", () => {
    const records = [
      sample(ME, 1),
      workspace(ME, "w1", "a"),
      terminal(ME, "t1", "w1", "running"),
      session(ME, "s1", "w1", "t1", "idle"),
      terminal(ME, "t2", "w1", "exited", { exit_code: 1 }),
      session(ME, "s2", "w1", "t2", "failed"),
      terminal(ME, "t3", "w1", "exited", { exit_code: 0 }),
      terminal(ME, "t4", "w1", "running"),
      terminal(ME, "t5", "w1", "unreachable"),
    ];
    const [w] = fleet(records, settings())[0].workspaces;
    expect(ended(w)).toEqual(["t2", "t3"]);
  });
});
