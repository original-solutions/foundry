import { describe, expect, it } from "vitest";
import { Backoff, socketUrl } from "./socket";

describe("socketUrl", () => {
  it("uses ws on the page's host for a page served over http", () => {
    expect(socketUrl("/api/events")).toBe(`ws://${location.host}/api/events`);
  });
});

describe("Backoff", () => {
  it("starts at 500 ms, doubles up to 5 s, and starts again after a reset", () => {
    const backoff = new Backoff();
    expect([1, 2, 3, 4, 5, 6].map(() => backoff.next())).toEqual([500, 1000, 2000, 4000, 5000, 5000]);
    backoff.reset();
    expect(backoff.next()).toBe(500);
  });
});
