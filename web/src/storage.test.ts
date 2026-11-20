import { afterEach, describe, expect, it, vi } from "vitest";
import { readStored, readStoredJson, store, storeJson, unstore } from "./storage";

afterEach(() => {
  vi.restoreAllMocks();
  localStorage.clear();
});

/** Makes every call on this browser's storage throw, as a blocked or full one does. */
function block() {
  for (const method of ["getItem", "setItem", "removeItem"] as const) {
    vi.spyOn(Storage.prototype, method).mockImplementation(() => {
      throw new DOMException("blocked", "SecurityError");
    });
  }
}

describe("storage", () => {
  it("keeps text and JSON for this browser, and takes a key out", () => {
    expect(store("a", "one")).toBe(true);
    expect(readStored("a")).toBe("one");
    expect(storeJson("b", { n: 1 })).toBe(true);
    expect(readStoredJson("b")).toEqual({ n: 1 });
    unstore("a");
    expect(readStored("a")).toBeNull();
  });

  it("reads nothing for a key never kept, or one that is not JSON", () => {
    expect(readStored("none")).toBeNull();
    expect(readStoredJson("none")).toBeUndefined();
    localStorage.setItem("bad", "{not json");
    expect(readStoredJson("bad")).toBeUndefined();
  });

  it("goes on when storage is blocked or full: reads nothing, keeps nothing and says so", () => {
    block();
    expect(readStored("a")).toBeNull();
    expect(readStoredJson("a")).toBeUndefined();
    expect(store("a", "one")).toBe(false);
    expect(storeJson("a", [1])).toBe(false);
    expect(() => unstore("a")).not.toThrow();
  });
});
