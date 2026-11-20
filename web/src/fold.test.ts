import { afterEach, describe, expect, it } from "vitest";
import { loadUnfolded, saveUnfolded } from "./fold";

afterEach(() => localStorage.clear());

describe("fold", () => {
  it("remembers the unfolded workspaces in this browser, and starts with none when nothing or garbage is kept", () => {
    expect(loadUnfolded()).toEqual([]);
    saveUnfolded(["w1", "w2"]);
    expect(loadUnfolded()).toEqual(["w1", "w2"]);
    localStorage.setItem("foundry.unfolded", "{not json");
    expect(loadUnfolded()).toEqual([]);
    localStorage.setItem("foundry.unfolded", JSON.stringify([1, "w3", null]));
    expect(loadUnfolded()).toEqual(["w3"]);
  });
});
