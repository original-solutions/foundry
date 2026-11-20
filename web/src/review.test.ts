import { afterEach, describe, expect, it } from "vitest";
import type { FileChange } from "./api";
import { diffLines } from "./changes";
import { freshLines, loadViewed, nextUnviewed, progress, saveViewed, seen, standing } from "./review";

const file = (path: string, patch: string | null, more: Partial<FileChange> = {}): FileChange => ({
  path,
  from: null,
  status: "modified",
  added: 1,
  removed: 0,
  uncommitted: true,
  patch,
  ...more,
});

const first = file("a.ts", "@@ -1,2 +1,2 @@\n keep\n-old\n+new\n");
const later = file("a.ts", "@@ -1,2 +1,3 @@\n keep\n-old\n+new\n+newer\n");

afterEach(() => localStorage.clear());

describe("review", () => {
  it("calls a file unviewed, viewed, or changed since it was viewed", () => {
    expect(standing(first, undefined)).toBe("unviewed");
    expect(standing(first, seen(first))).toBe("viewed");
    expect(standing(later, seen(first))).toBe("changed");
  });

  it("tells a binary file's change by its status and counts", () => {
    const png = file("logo.png", null, { added: null, removed: null, status: "added" });
    expect(standing(png, seen(png))).toBe("viewed");
    expect(standing({ ...png, status: "modified" }, seen(png))).toBe("changed");
  });

  it("marks the changed lines that were not there when the file was viewed", () => {
    const lines = diffLines(later.patch!);
    const fresh = freshLines(lines, seen(first));
    expect([...fresh].map((i) => lines[i])).toEqual([{ kind: "added", text: "newer", new: 3 }]);
  });

  it("marks nothing for a file never viewed", () => {
    expect(freshLines(diffLines(later.patch!), undefined).size).toBe(0);
  });

  it("counts viewed files, and changed ones apart", () => {
    const other = file("b.ts", "@@ -0,0 +1 @@\n+x\n");
    const third = file("c.ts", "@@ -0,0 +1 @@\n+y\n");
    expect(progress([later, other, third], { "a.ts": seen(first), "b.ts": seen(other) })).toEqual({ total: 3, viewed: 1, changed: 1 });
  });

  it("finds the next file still to view after a given one, wrapping round", () => {
    const files = [file("a", "+1"), file("b", "+2"), file("c", "+3")];
    const viewed = { b: seen(files[1]) };
    expect(nextUnviewed(files, viewed, 0)).toBe(2);
    expect(nextUnviewed(files, viewed, 2)).toBe(0);
    expect(nextUnviewed(files, { a: seen(files[0]), b: seen(files[1]), c: seen(files[2]) }, 0)).toBeUndefined();
  });

  it("keeps what was viewed per workspace in this browser, dropping files no longer changed", () => {
    saveViewed("w1", { "a.ts": seen(first), "gone.ts": seen(first) }, [first]);
    saveViewed("w2", { "a.ts": seen(later) }, [later]);
    expect(Object.keys(loadViewed("w1"))).toEqual(["a.ts"]);
    expect(standing(first, loadViewed("w1")["a.ts"])).toBe("viewed");
    expect(standing(first, loadViewed("w2")["a.ts"])).toBe("changed");
    expect(loadViewed("w3")).toEqual({});
  });

  it("forgets the workspaces viewed longest ago past forty", () => {
    for (let i = 0; i < 45; i++) saveViewed(`w${i}`, { "a.ts": seen(first) }, [first]);
    expect(loadViewed("w0")).toEqual({});
    expect(loadViewed("w44")["a.ts"]).toBeDefined();
    saveViewed("w5", { "a.ts": seen(first) }, [first]);
    saveViewed("w45", { "a.ts": seen(first) }, [first]);
    expect(loadViewed("w5")["a.ts"]).toBeDefined();
    expect(loadViewed("w6")).toEqual({});
  });

  it("starts empty when storage holds something else", () => {
    localStorage.setItem("foundry.viewed", "not json");
    expect(loadViewed("w1")).toEqual({});
    localStorage.setItem("foundry.viewed", JSON.stringify({ w1: { "a.ts": 3 } }));
    expect(loadViewed("w1")).toEqual({});
  });
});
