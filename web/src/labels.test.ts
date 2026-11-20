import { describe, expect, it } from "vitest";
import type { SessionState } from "./api";
import { addsSteps, busy, needsYou, plural, sessionLabel, sessionGlyph, terminalLabel, terminalGlyph, urgency, type Glyph } from "./labels";

describe("labels", () => {
  it("names every session state", () => {
    const states: SessionState[] = ["starting", "working", "waiting_for_you", "idle", "finished", "failed", "stuck", "unreachable"];
    expect(states.map((s) => sessionLabel[s])).toEqual([
      "Starting",
      "Working",
      "Waiting for you",
      "Idle",
      "Finished",
      "Failed",
      "Stuck",
      "Unreachable",
    ]);
  });

  it("names every terminal state, with the exit code when known", () => {
    expect(terminalLabel("running", null)).toBe("Running");
    expect(terminalLabel("exited", null)).toBe("Exited");
    expect(terminalLabel("exited", 1)).toBe("Exited 1");
    expect(terminalLabel("exited", 0)).toBe("Exited 0");
    expect(terminalLabel("unreachable", null)).toBe("Unreachable");
  });

  it("gives every state a glyph: one per meaning, shared by sessions and shells", () => {
    const states: SessionState[] = ["starting", "working", "waiting_for_you", "idle", "finished", "failed", "stuck", "unreachable"];
    expect(states.map((s) => sessionGlyph[s])).toEqual(["starting", "working", "waiting", "ready", "done", "failed", "stuck", "away"]);
    expect(terminalGlyph("running", null)).toBe("ready");
    expect(terminalGlyph("exited", 0)).toBe("done");
    expect(terminalGlyph("exited", null)).toBe("done");
    expect(terminalGlyph("exited", 1)).toBe("failed");
    expect(terminalGlyph("unreachable", null)).toBe("away");
  });
});

describe("shared glyph sets", () => {
  const glyphs: Glyph[] = ["starting", "working", "waiting", "ready", "done", "failed", "stuck", "away"];

  it("ranks every glyph exactly once, what needs a person first, then what is busy", () => {
    expect([...urgency].sort()).toEqual([...glyphs].sort());
    expect(urgency.slice(0, needsYou.length)).toEqual(needsYou);
    expect(urgency).toEqual(["waiting", "stuck", "failed", "working", "starting", "ready", "done", "away"]);
  });

  it("counts a session as busy for its whole turn, and as adding steps on its own only while it starts or works", () => {
    expect(busy).toEqual(["starting", "working", "waiting", "stuck"]);
    expect(addsSteps).toEqual(["starting", "working"]);
    expect(addsSteps.every((g) => busy.includes(g))).toBe(true);
  });
});

describe("plural", () => {
  it("counts a thing in plain words", () => {
    expect(plural(1, "file")).toBe("1 file");
    expect(plural(0, "file")).toBe("0 files");
    expect(plural(3, "commit")).toBe("3 commits");
  });
});
