import { describe, expect, it } from "vitest";
import { ApiError } from "./api";
import { pollEvery, RETRY_EVERY } from "./poll";

const read = (status: "success" | "error" | "pending", error: Error | null = null) => ({ state: { status, error } });

describe("pollEvery", () => {
  it("reads every `every` while live, and not on a timer while quiet", () => {
    expect(pollEvery(2000, true)(read("success"))).toBe(2000);
    expect(pollEvery(2000, false)(read("success"))).toBe(false);
  });

  it("keeps reading after a failed read, backing off, never faster than it was", () => {
    expect(pollEvery(2000, true)(read("error", new ApiError("daemon hiccup", 500)))).toBe(RETRY_EVERY);
    expect(pollEvery(2000, true)(read("error", new TypeError("network down")))).toBe(RETRY_EVERY);
    expect(pollEvery(60_000, true)(read("error", new ApiError("daemon hiccup", 500)))).toBe(60_000);
    expect(RETRY_EVERY).toBeGreaterThan(2000);
  });

  it("stops on a 404: what it reads is another machine's, or gone", () => {
    expect(pollEvery(2000, true)(read("error", new ApiError("no session here", 404)))).toBe(false);
  });
});
