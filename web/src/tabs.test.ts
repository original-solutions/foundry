import { afterEach, describe, expect, it } from "vitest";
import { ATTACHED_MAX, loadTabs, saveTabs, touched, withoutTab, withTab } from "./tabs";

afterEach(() => localStorage.clear());

describe("tabs", () => {
  it("adds a terminal at the end once", () => {
    expect(withTab([], "t1")).toEqual(["t1"]);
    expect(withTab(["t1", "t2"], "t3")).toEqual(["t1", "t2", "t3"]);
    const same = ["t1", "t2"];
    expect(withTab(same, "t1")).toBe(same);
  });

  it("closes a tab and names the one to show next: its right neighbour, else its left, else none", () => {
    expect(withoutTab(["t1", "t2", "t3"], "t2")).toEqual({ tabs: ["t1", "t3"], next: "t3" });
    expect(withoutTab(["t1", "t2", "t3"], "t3")).toEqual({ tabs: ["t1", "t2"], next: "t2" });
    expect(withoutTab(["t1"], "t1")).toEqual({ tabs: [], next: undefined });
  });

  it("remembers the open tabs in this browser, and starts empty when nothing or garbage is kept", () => {
    expect(loadTabs()).toEqual([]);
    saveTabs(["t1", "t2"]);
    expect(loadTabs()).toEqual(["t1", "t2"]);
    localStorage.setItem("foundry.tabs", "{not json");
    expect(loadTabs()).toEqual([]);
    localStorage.setItem("foundry.tabs", JSON.stringify([1, "t3", null]));
    expect(loadTabs()).toEqual(["t3"]);
  });

  it("keeps the panes used last attached, most recent first, up to the cap", () => {
    expect(ATTACHED_MAX).toBe(6);
    expect(touched([], "t1")).toEqual(["t1"]);
    expect(touched(["t1", "t2"], "t2")).toEqual(["t2", "t1"]);
    expect(touched(["t6", "t5", "t4", "t3", "t2", "t1"], "t7")).toEqual(["t7", "t6", "t5", "t4", "t3", "t2"]);
    expect(touched(["t3", "t2", "t1"], "t4", [], 3)).toEqual(["t4", "t3", "t2"]);
  });

  it("never drops a pane shown in a split, however long ago it was used", () => {
    expect(touched(["t3", "t2", "t1"], "t4", ["t4", "t1"], 3)).toEqual(["t4", "t3", "t1"]);
    expect(touched(["t3", "t2", "t1"], "t4", ["t4", "t1"], 2)).toEqual(["t4", "t1"]);
  });
});
