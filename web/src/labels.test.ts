import { describe, expect, it } from "vitest";
import type { SessionState } from "./api";
import { addsSteps, busy, couldNot, needsYou, plural, sessionLabel, sessionGlyph, terminalLabel, terminalGlyph, urgency, words, type Glyph } from "./labels";

describe("labels", () => {
  it("names every session state in the words the board's lanes and the first-run page use", () => {
    const states: SessionState[] = ["starting", "working", "waiting_for_you", "idle", "finished", "failed", "stuck", "unreachable"];
    expect(states.map((s) => sessionLabel[s])).toEqual(["Starting", "Working", "Needs you", "Ready", "Ended", "Failed", "Stuck", "Unreachable"]);
  });

  it("names a shell's end as a session's is named: ended, or failed with its exit code", () => {
    expect(terminalLabel("running", null)).toBe("Running");
    expect(terminalLabel("exited", null)).toBe("Ended");
    expect(terminalLabel("exited", 0)).toBe("Ended");
    expect(terminalLabel("exited", 1)).toBe("Failed · exit 1");
    expect(terminalLabel("unreachable", null)).toBe("Unreachable");
  });

  it("gives a label the same word for every state that shares a glyph's meaning", () => {
    expect(terminalLabel("exited", 0)).toBe(sessionLabel.finished);
    expect(terminalLabel("exited", 2).startsWith(sessionLabel.failed)).toBe(true);
    expect(terminalLabel("unreachable", null)).toBe(sessionLabel.unreachable);
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

describe("couldNot", () => {
  it("says what could not be done, then why, as one sentence", () => {
    expect(couldNot("start a shell", new Error("workspace not found"))).toBe("Could not start a shell: workspace not found.");
    expect(couldNot("end opus 2", new Error("the terminal is gone."))).toBe("Could not end opus 2: the terminal is gone.");
    expect(couldNot("read the changes", "timed out")).toBe("Could not read the changes: timed out.");
  });
});

describe("words", () => {
  it("keeps the phrases several screens repeat in one place", () => {
    expect(words.noChanges).toBe("No changes yet");
    expect(words.unreadable).toBe("Its changes can't be read from here");
    expect(words.changes).toMatch(/^What this workspace changed since/);
    expect(words.archive).toMatch(/^Archive: /);
  });
});

describe("plural", () => {
  it("counts a thing in plain words", () => {
    expect(plural(1, "file")).toBe("1 file");
    expect(plural(0, "file")).toBe("0 files");
    expect(plural(3, "commit")).toBe("3 commits");
  });
});
