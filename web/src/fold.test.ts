import { afterEach, describe, expect, it } from "vitest";
import { loadFolded, saveFolded } from "./fold";

afterEach(() => localStorage.clear());

describe("fold", () => {
  it("remembers the folded workspaces in this browser, and starts with none when nothing or garbage is kept", () => {
    expect(loadFolded()).toEqual([]);
    saveFolded(["w1", "w2"]);
    expect(loadFolded()).toEqual(["w1", "w2"]);
    localStorage.setItem("foundry.folded", "{not json");
    expect(loadFolded()).toEqual([]);
    localStorage.setItem("foundry.folded", JSON.stringify([1, "w3", null]));
    expect(loadFolded()).toEqual(["w3"]);
  });
});
