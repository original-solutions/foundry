import { afterEach, describe, expect, it, vi } from "vitest";
import { keep, MEMORY_KEY, recall } from "./memory";
import { ME, sample, session, settings, terminal, workspace } from "./test-fixtures";

afterEach(() => {
  localStorage.clear();
  vi.restoreAllMocks();
});

const records = [sample(ME, 3), workspace(ME, "w1", "fix login"), terminal(ME, "t1", "w1"), session(ME, "s1", "w1", "t1", "working")];

describe("memory", () => {
  it("keeps the fleet the daemon last sent in this browser, with when, and gives it back", () => {
    expect(recall()).toBeUndefined();
    keep({ records, settings: settings(), synced: true }, 1_000);
    expect(recall()).toEqual({ at: 1_000, records, settings: settings() });
  });

  it("keeps nothing before the first full state, when an empty list does not mean an empty fleet", () => {
    keep({ records: [], settings: settings(), synced: false }, 1_000);
    expect(localStorage.getItem(MEMORY_KEY)).toBeNull();
  });

  it("gives back nothing for garbage, another shape or a version this page does not read", () => {
    localStorage.setItem(MEMORY_KEY, "{not json");
    expect(recall()).toBeUndefined();
    localStorage.setItem(MEMORY_KEY, JSON.stringify({ v: 1, at: 5, records: "no", settings: settings() }));
    expect(recall()).toBeUndefined();
    localStorage.setItem(MEMORY_KEY, JSON.stringify({ v: 99, at: 5, records, settings: settings() }));
    expect(recall()).toBeUndefined();
  });

  it("does without when storage is blocked or full", () => {
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("QuotaExceededError");
    });
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("SecurityError");
    });
    expect(() => keep({ records, settings: settings(), synced: true }, 1)).not.toThrow();
    expect(recall()).toBeUndefined();
  });
});
