import { describe, expect, it } from "vitest";
import { heardAgo, remember, scaled, SAMPLES_WINDOW, span, spanWords, type History } from "./samples";
import { ME, OTHER, sample, workspace } from "./test-fixtures";

describe("remember", () => {
  it("keeps each machine's samples in the order they arrive, one point per new version", () => {
    let h: History = {};
    h = remember(h, [sample(ME, 1, { load: 0.2, mem: 8e9 }), sample(OTHER, 4, { load: 1.1 })], 1_000);
    h = remember(h, [sample(ME, 2, { load: 0.5, mem: 7e9 }), sample(OTHER, 4, { load: 1.1 })], 6_000);
    expect(h[ME].points).toEqual([
      { version: 1, at: 1_000, load: 0.2, memory: 8e9 },
      { version: 2, at: 6_000, load: 0.5, memory: 7e9 },
    ]);
    expect(h[OTHER].points.map((p) => p.version)).toEqual([4]);
  });

  it("gives back the same history when no sample is new, so nothing renders again", () => {
    const h = remember({}, [sample(ME, 1)], 1_000);
    expect(remember(h, [sample(ME, 1), workspace(ME, "w1", "x")], 2_000)).toBe(h);
    expect(remember(h, [sample(ME, 0)], 3_000)).toBe(h);
  });

  it("forgets points older than the window", () => {
    let h = remember({}, [sample(ME, 1)], 0);
    h = remember(h, [sample(ME, 2)], SAMPLES_WINDOW / 2);
    h = remember(h, [sample(ME, 3)], SAMPLES_WINDOW + 1);
    expect(h[ME].points.map((p) => p.version)).toEqual([2, 3]);
  });

  it("says a machine was heard only once a newer sample arrives than the one it was first seen with", () => {
    // The first sample may be old news (an offline machine's last word), so it is not "heard".
    let h = remember({}, [sample(ME, 7)], 1_000);
    expect(h[ME].heard).toBeUndefined();
    h = remember(h, [sample(ME, 8)], 6_000);
    expect(h[ME].heard).toBe(6_000);
  });
});

describe("heardAgo", () => {
  it("says how long since the machine was heard in words a glance can take", () => {
    expect(heardAgo(undefined, 5_000)).toBeUndefined();
    expect(heardAgo(5_000, 5_400)).toBe("just now");
    expect(heardAgo(5_000, 9_000)).toBe("4 s ago");
    expect(heardAgo(0, 125_000)).toBe("2 min ago");
    expect(heardAgo(0, 2 * 3_600_000 + 5)).toBe("2 h ago");
  });
});

describe("scaled", () => {
  it("places points by time across the window and by value up to the top of the scale", () => {
    const points = [
      { version: 1, at: 0, load: 0, memory: 0 },
      { version: 2, at: SAMPLES_WINDOW / 2, load: 1, memory: 0 },
      { version: 3, at: SAMPLES_WINDOW, load: 2, memory: 0 },
    ];
    expect(scaled(points, (p) => p.load, 2, SAMPLES_WINDOW, SAMPLES_WINDOW)).toEqual([
      [0, 1],
      [0.5, 0.5],
      [1, 0],
    ]);
  });

  it("pins a value over the top of the scale to the top", () => {
    expect(scaled([{ version: 1, at: 10, load: 9, memory: 0 }], (p) => p.load, 1, 10, SAMPLES_WINDOW)).toEqual([[1, 0]]);
  });
});

describe("span", () => {
  it("covers the points this page has, at least a minute and at most the window, so a new page draws a whole line", () => {
    const at = (t: number) => ({ version: t, at: t, load: 0, memory: 0 });
    expect(span([], 5_000)).toBe(60_000);
    expect(span([at(0), at(5_000)], 20_000)).toBe(60_000);
    expect(span([at(0), at(5_000)], 150_000)).toBe(150_000);
    expect(span([at(0)], SAMPLES_WINDOW * 3)).toBe(SAMPLES_WINDOW);
  });

  it("names its length", () => {
    expect(spanWords(60_000)).toBe("last 1 min");
    expect(spanWords(150_000)).toBe("last 3 min");
    expect(spanWords(SAMPLES_WINDOW)).toBe("last 10 min");
  });
});
