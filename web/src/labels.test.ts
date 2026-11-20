import { describe, expect, it } from "vitest";
import type { SessionState } from "./api";
import { sessionLabel, sessionTone, terminalLabel, terminalTone } from "./labels";

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

  it("gives every state a tone: what needs you stands out, what failed is red, what is away is dim", () => {
    const states: SessionState[] = ["starting", "working", "waiting_for_you", "idle", "finished", "failed", "stuck", "unreachable"];
    expect(states.map((s) => sessionTone[s])).toEqual(["quiet", "busy", "attention", "ready", "quiet", "bad", "bad", "away"]);
    expect(terminalTone("running", null)).toBe("ready");
    expect(terminalTone("exited", 0)).toBe("quiet");
    expect(terminalTone("exited", null)).toBe("quiet");
    expect(terminalTone("exited", 1)).toBe("bad");
    expect(terminalTone("unreachable", null)).toBe("away");
  });
});
