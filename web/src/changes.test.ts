import { describe, expect, it } from "vitest";
import type { Changes } from "./api";
import { diffLines, leading, splitPath, totals } from "./changes";

const changes: Changes = {
  base: "main",
  commits: [{ hash: "abc1234", subject: "add search" }],
  files: [
    { path: "src/a.ts", from: null, status: "modified", added: 3, removed: 1, uncommitted: true, patch: "" },
    { path: "logo.png", from: null, status: "added", added: null, removed: null, uncommitted: false, patch: null },
    { path: "notes.md", from: null, status: "untracked", added: 2, removed: 0, uncommitted: true, patch: "" },
  ],
};

describe("changes", () => {
  it("totals lines and files, leaving binary files out of the line counts", () => {
    expect(totals(changes)).toEqual({ files: 3, added: 5, removed: 1, uncommitted: 2 });
  });

  it("numbers each line from its hunk header, old on the left and new on the right", () => {
    const patch = "@@ -10,3 +10,4 @@ fn main() {\n keep\n-old\n+new\n+more\n keep\n\\ No newline at end of file\n@@ -40 +41 @@\n-x\n+y\n";
    expect(diffLines(patch)).toEqual([
      { kind: "hunk", text: "@@ -10,3 +10,4 @@ fn main() {" },
      { kind: "same", text: "keep", old: 10, new: 10 },
      { kind: "removed", text: "old", old: 11 },
      { kind: "added", text: "new", new: 11 },
      { kind: "added", text: "more", new: 12 },
      { kind: "same", text: "keep", old: 12, new: 13 },
      { kind: "note", text: "No newline at end of file" },
      { kind: "hunk", text: "@@ -40 +41 @@" },
      { kind: "removed", text: "x", old: 40 },
      { kind: "added", text: "y", new: 41 },
    ]);
  });

  it("splits a path into its folder and its name", () => {
    expect(splitPath("web/src/App.tsx")).toEqual({ dir: "web/src/", name: "App.tsx" });
    expect(splitPath("README.md")).toEqual({ dir: "", name: "README.md" });
  });
});

describe("leading", () => {
  const lines = diffLines(["@@ -1,4 +1,4 @@", " a", "-b", "-c", "+B", "+C", " d", "\\ No newline at end of file", " e"].join("\n"));

  it("stops once it holds the lines asked for, notes not counted", () => {
    expect(leading(lines, 2).map((l) => l.text)).toEqual(["@@ -1,4 +1,4 @@", "a"]);
    expect(leading(lines, 7).map((l) => l.text)).toEqual(["@@ -1,4 +1,4 @@", "a", "b", "c", "B", "C", "d"]);
  });

  it("carries on to the end of a change it stops inside, so removed lines meet their added ones", () => {
    expect(leading(lines, 3).map((l) => l.text)).toEqual(["@@ -1,4 +1,4 @@", "a", "b", "c", "B", "C"]);
  });

  it("is the whole patch when it is shorter than asked", () => {
    expect(leading(lines, 50)).toEqual(lines);
  });
});
