import { describe, expect, it } from "vitest";
import { listOf } from "./tasks";

describe("listOf", () => {
  it("reads a task written as a list of dashes, stars, bullets or numbers as its items", () => {
    expect(listOf("- Fix the login redirect\n- Add CSV export")).toEqual(["Fix the login redirect", "Add CSV export"]);
    expect(listOf("* one\n* two\n* three")).toEqual(["one", "two", "three"]);
    expect(listOf("• one\n• two")).toEqual(["one", "two"]);
    expect(listOf("1. one\n2) two\n10. ten")).toEqual(["one", "two", "ten"]);
  });

  it("skips blank lines and keeps an indented line with the item above it", () => {
    expect(listOf("\n- Fix the footer\n  It overlaps the cart on a phone.\n\n- Tidy the README\n")).toEqual([
      "Fix the footer\nIt overlaps the cart on a phone.",
      "Tidy the README",
    ]);
  });

  it("is no list for one item, for prose, or for prose around items", () => {
    expect(listOf("- Fix the footer")).toBeUndefined();
    expect(listOf("Fix the footer.\nThen tidy the README.")).toBeUndefined();
    expect(listOf("Do these:\n- one\n- two")).toBeUndefined();
    expect(listOf("- one\n- two\nand keep it short")).toBeUndefined();
    expect(listOf("")).toBeUndefined();
  });

  it("is no list when an item has nothing after its mark", () => {
    expect(listOf("- one\n-\n- two")).toBeUndefined();
  });
});
