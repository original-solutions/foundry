import { describe, expect, it } from "vitest";
import { diffLines } from "./changes";
import { changedWords, paint, type Piece } from "./paint";

/** A line's pieces as text, with a changed piece in [brackets] and a coloured one as `tone:text`. */
function shown(pieces: Piece[]) {
  return pieces.map((p) => `${p.changed ? "[" : ""}${p.tone ? `${p.tone}:` : ""}${p.text}${p.changed ? "]" : ""}`).join("|");
}

describe("changedWords", () => {
  it("marks only the words that differ between a line and the line that replaced it", () => {
    expect(changedWords("const b = 2;", "const b = 3;")).toEqual([[[10, 11]], [[10, 11]]]);
    expect(changedWords("  return limit;", "  return limits;")).toEqual([[[9, 14]], [[9, 15]]]);
    expect(changedWords("let a = old(1);", "let b = old(2);")).toEqual([
      [
        [4, 5],
        [12, 13],
      ],
      [
        [4, 5],
        [12, 13],
      ],
    ]);
  });

  it("marks one side only when the other just lost or gained text", () => {
    expect(changedWords("f(a);", "f(a, b);")).toEqual([[], [[3, 6]]]);
  });

  it("marks nothing when the lines have too little in common to pair", () => {
    expect(changedWords("import { x } from './x';", "export default function main() {}")).toBeUndefined();
    expect(changedWords("    a", "    b")).toBeUndefined();
  });
});

describe("paint", () => {
  const patch = "@@ -1,4 +1,4 @@\n /* keep\n-   the old */ const b = 2;\n+   the new */ const b = 3;\n const c = 4;\n";

  it("colours each line as code, carrying a comment across lines on each side, and marks the words that changed", () => {
    const lines = diffLines(patch);
    expect(paint(lines, "src/a.ts").map(shown)).toEqual([
      "",
      "comment:/* keep",
      "comment:   the |[comment:old]|comment: */| |keyword:const| b = |[number:2]|;",
      "comment:   the |[comment:new]|comment: */| |keyword:const| b = |[number:3]|;",
      "keyword:const| c = |number:4|;",
    ]);
  });

  it("marks changed words in a file it cannot colour, and leaves the lines plain", () => {
    const lines = diffLines("@@ -1 +1 @@\n-hello world\n+hello there\n");
    expect(paint(lines, "NOTES").map(shown)).toEqual(["", "hello |[world]", "hello |[there]"]);
  });

  it("pairs each removed line with the next added line after it that is alike, in order, past a line put in between", () => {
    const lines = diffLines("@@ -1,2 +1,3 @@\n-function total(items) {\n-  return sum(items);\n+/** The cart's total, in cents. */\n+function total(items, discount) {\n+  return sum(items) - discount;\n");
    expect(paint(lines, "x.txt").map(shown)).toEqual([
      "",
      "function total(items) {",
      "  return sum(items);",
      "/** The cart's total, in cents. */",
      "function total(items|[, discount]|) {",
      "  return sum(items) |[- discount]|;",
    ]);
  });

  it("pairs a run of removed lines with the added run after it, line by line, and leaves an unpaired line whole", () => {
    const lines = diffLines("@@ -1,2 +1,3 @@\n-let a = 1;\n-let b = 1;\n+let a = 2;\n+let b = 2;\n+let c = 2;\n");
    expect(paint(lines, "x.txt").map(shown)).toEqual(["", "let a = |[1]|;", "let b = |[1]|;", "let a = |[2]|;", "let b = |[2]|;", "let c = 2;"]);
  });

  it("starts each hunk afresh: a comment left open in one hunk does not colour the next", () => {
    const lines = diffLines("@@ -1 +1 @@\n /* open\n@@ -9 +9 @@\n done();\n");
    expect(paint(lines, "a.ts").map(shown)).toEqual(["", "comment:/* open", "", "call:done|();"]);
  });
});
