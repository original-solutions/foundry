import { describe, expect, it } from "vitest";
import { beside, follow } from "./split";

describe("split", () => {
  it("puts a terminal beside the open one, on the right, and replaces whatever was beside it", () => {
    expect(beside(undefined, "t1", "t2")).toEqual({ left: "t1", right: "t2" });
    expect(beside({ left: "t1", right: "t2" }, "t1", "t3")).toEqual({ left: "t1", right: "t3" });
    expect(beside({ left: "t1", right: "t2" }, "t2", "t3")).toEqual({ left: "t3", right: "t2" });
  });

  it("keeps both sides when focus moves between them", () => {
    const split = { left: "t1", right: "t2" };
    expect(follow(split, "t1", "t2")).toBe(split);
    expect(follow(split, "t2", "t1")).toBe(split);
  });

  it("shows a terminal opened from anywhere in the side that had focus", () => {
    expect(follow({ left: "t1", right: "t2" }, "t1", "t3")).toEqual({ left: "t3", right: "t2" });
    expect(follow({ left: "t1", right: "t2" }, "t2", "t3")).toEqual({ left: "t1", right: "t3" });
  });

  it("ends the split on the fleet view, or when focus was on neither side", () => {
    expect(follow({ left: "t1", right: "t2" }, "t1", undefined)).toBeUndefined();
    expect(follow({ left: "t1", right: "t2" }, "t9", "t3")).toBeUndefined();
    expect(follow(undefined, "t1", "t3")).toBeUndefined();
  });
});
