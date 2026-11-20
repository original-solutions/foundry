import { describe, expect, it } from "vitest";
import { closest, hitsIn, MAX_HITS, marks, step } from "./find";

const rows = (...lines: (string | [string, "wrapped"])[]) => lines.map((l) => (typeof l === "string" ? { text: l, wrapped: false } : { text: l[0], wrapped: true }));

describe("find", () => {
  it("finds every hit in every row, oldest first, with its row, column and length", () => {
    expect(hitsIn(rows("error one", "fine", "an error, another error"), "error")).toEqual([
      { row: 0, col: 0, length: 5 },
      { row: 2, col: 3, length: 5 },
      { row: 2, col: 18, length: 5 },
    ]);
  });

  it("ignores case for a lower-case query, and matches case once the query has a capital", () => {
    const screen = rows("Error", "error", "ERROR");
    expect(hitsIn(screen, "error").map((h) => h.row)).toEqual([0, 1, 2]);
    expect(hitsIn(screen, "Error").map((h) => h.row)).toEqual([0]);
  });

  it("finds a hit that a long line wraps across two rows, at the row it starts on", () => {
    const screen = rows("$ cargo te", ["st --workspace", "wrapped"], "done");
    expect(hitsIn(screen, "cargo test")).toEqual([{ row: 0, col: 2, length: 10 }]);
    expect(hitsIn(screen, "workspace")).toEqual([{ row: 1, col: 5, length: 9 }]);
  });

  it("does not match the blank cells that pad a row, or across two separate lines", () => {
    expect(hitsIn(rows("ab   ", "cd"), "b c")).toEqual([]);
    expect(hitsIn(rows("ab   ", "cd"), "b ")).toEqual([]);
  });

  it("takes the query literally, not as a pattern", () => {
    expect(hitsIn(rows("a.b axb (x)"), "a.b")).toEqual([{ row: 0, col: 0, length: 3 }]);
    expect(hitsIn(rows("a.b axb (x)"), "(x)")).toEqual([{ row: 0, col: 8, length: 3 }]);
  });

  it("finds nothing for an empty query", () => {
    expect(hitsIn(rows("anything"), "")).toEqual([]);
  });

  it("stops at a cap, so a one-letter query over 10,000 lines stays quick", () => {
    const many = rows(...Array.from({ length: 10_000 }, () => "eeeeeeeeee"));
    expect(hitsIn(many, "e")).toHaveLength(MAX_HITS);
  });

  it("steps through hits both ways and wraps around", () => {
    expect(step(0, 3, -1)).toBe(2);
    expect(step(2, 3, 1)).toBe(0);
    expect(step(1, 3, 1)).toBe(2);
  });

  it("keeps the same hit after the screen grows, or the nearest one when it scrolled away", () => {
    const hits = [
      { row: 2, col: 0, length: 1 },
      { row: 5, col: 4, length: 1 },
      { row: 9, col: 0, length: 1 },
    ];
    expect(closest(hits, { row: 5, col: 4, length: 1 })).toBe(1);
    expect(closest(hits, { row: 8, col: 0, length: 1 })).toBe(2);
    expect(closest(hits, { row: 0, col: 0, length: 1 })).toBe(0);
  });

  it("marks where the hits sit in the whole scrollback, one mark per spot leading to its first hit, the current one lit", () => {
    const hits = [
      { row: 0, col: 0, length: 1 },
      { row: 0, col: 4, length: 1 },
      { row: 500, col: 0, length: 1 },
      { row: 999, col: 0, length: 1 },
    ];
    expect(marks(hits, 1000, 2)).toEqual([
      { at: 0, hit: 0, current: false },
      { at: 50, hit: 2, current: true },
      { at: 99.9, hit: 3, current: false },
    ]);
  });
});
