import { describe, expect, it } from "vitest";
import type { SessionState } from "./api";
import { sessionLabel, sessionGlyph, terminalLabel, terminalGlyph } from "./labels";

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
