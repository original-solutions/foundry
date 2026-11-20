import { describe, expect, it } from "vitest";
import type { Changes, PullRequest, Shipping } from "./api";
import { stageIndex, stageOf } from "./stage";

const changes = (commits: number, files: number): Changes => ({
  base: "main",
  commits: Array.from({ length: commits }, (_, i) => ({ hash: `h${i}`, subject: `commit ${i}` })),
  files: Array.from({ length: files }, (_, i) => ({ path: `f${i}`, from: null, status: "modified", added: 1, removed: 0, uncommitted: commits === 0, patch: "" })),
});
const pr = (state: PullRequest["state"]): PullRequest => ({ number: 7, title: "t", url: "https://github.com/o/r/pull/7", state, draft: false, checks: "none", review: "none" });
const shipping = (extra: Partial<Shipping> = {}): Shipping => ({ branch: "b", pushed: false, unpushed: 0, pr: null, unreachable: false, ...extra });

describe("stageOf", () => {
  it("is none for a workspace that changed nothing", () => {
    expect(stageOf(changes(0, 0), shipping())).toBe("none");
    expect(stageOf(changes(0, 0))).toBe("none");
  });

  it("is here for edits or commits GitHub does not have", () => {
    expect(stageOf(changes(0, 2), shipping())).toBe("here");
    expect(stageOf(changes(2, 2), shipping({ unpushed: 2 }))).toBe("here");
    // Where the branch stands is not known yet: its own edits are still only here.
    expect(stageOf(changes(1, 1))).toBe("here");
  });

  it("is pushed once GitHub has the branch's own commits, even with more to push", () => {
    expect(stageOf(changes(2, 2), shipping({ pushed: true }))).toBe("pushed");
    expect(stageOf(changes(3, 2), shipping({ pushed: true, unpushed: 1 }))).toBe("pushed");
    // A branch pushed with no commits of its own has nothing on GitHub to show.
    expect(stageOf(changes(0, 1), shipping({ pushed: true }))).toBe("here");
  });

  it("follows the pull request: open is in review, merged is merged, closed falls back to pushed", () => {
    expect(stageOf(changes(1, 1), shipping({ pushed: true, pr: pr("open") }))).toBe("review");
    expect(stageOf(changes(1, 1), shipping({ pushed: true, pr: pr("merged") }))).toBe("merged");
    expect(stageOf(changes(0, 0), shipping({ pushed: true, pr: pr("merged") }))).toBe("merged");
    expect(stageOf(changes(1, 1), shipping({ pushed: true, pr: pr("closed") }))).toBe("pushed");
  });

  it("orders the stages from here to merged", () => {
    expect(stageIndex("here")).toBeLessThan(stageIndex("pushed"));
    expect(stageIndex("pushed")).toBeLessThan(stageIndex("review"));
    expect(stageIndex("review")).toBeLessThan(stageIndex("merged"));
  });
});
