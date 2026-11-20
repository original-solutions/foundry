import { describe, expect, it } from "vitest";
import type { Mark } from "./api";
import { ago, kindOf, tally, took, turns } from "./trail";

const mark = (kind: Mark["kind"], text: string, at: number): Mark => ({ kind, text, at });

describe("turns", () => {
  it("splits the trail at each prompt, oldest turn first, and ends a turn at its done", () => {
    const trail = [
      mark("prompt", "fix the bug", 1000),
      mark("tool", "Read a.rs", 2000),
      mark("tool", "$ cargo test", 5000),
      mark("done", "", 9000),
      mark("prompt", "add a test", 20_000),
      mark("ask", "Claude needs your permission to use Bash", 21_000),
    ];
    const [before, now] = turns(trail);
    expect(before).toEqual({ prompt: "fix the bug", steps: trail.slice(1, 3), start: 1000, end: 9000 });
    expect(now).toEqual({ prompt: "add a test", steps: [trail[5]], start: 20_000, end: undefined });
  });

  it("keeps steps from before the first prompt it kept as a turn of their own", () => {
    const [only] = turns([mark("tool", "Edit x.md", 10), mark("done", "", 30)]);
    expect(only).toEqual({ prompt: undefined, steps: [mark("tool", "Edit x.md", 10)], start: 10, end: 30 });
  });

  it("is empty for an empty trail", () => {
    expect(turns([])).toEqual([]);
  });
});

describe("kindOf", () => {
  it("names what a tool step did from its text", () => {
    expect(kindOf(mark("tool", "$ ls", 0))).toBe("command");
    expect(kindOf(mark("tool", "Edit README.md", 0))).toBe("edit");
    expect(kindOf(mark("tool", "Write a.ts", 0))).toBe("edit");
    expect(kindOf(mark("tool", "MultiEdit a.ts", 0))).toBe("edit");
    expect(kindOf(mark("tool", "Read a.ts", 0))).toBe("read");
    expect(kindOf(mark("tool", "Grep fn main", 0))).toBe("search");
    expect(kindOf(mark("tool", "Glob **/*.md", 0))).toBe("search");
    expect(kindOf(mark("tool", "Fetch https://x.dev", 0))).toBe("web");
    expect(kindOf(mark("tool", "Agent: find it", 0))).toBe("agent");
    expect(kindOf(mark("tool", "TodoWrite", 0))).toBe("other");
    expect(kindOf(mark("ask", "Claude needs you", 0))).toBe("ask");
  });
});

describe("tally", () => {
  it("counts a turn's steps by what they did, biggest first, in plain words", () => {
    const steps = [mark("tool", "$ ls", 0), mark("tool", "Edit a", 0), mark("tool", "$ npm test", 0), mark("tool", "Read b", 0), mark("ask", "ok?", 0)];
    expect(tally(steps)).toBe("2 commands · 1 edit · 1 read · 1 question");
    expect(tally([])).toBe("no tools");
  });
});

describe("times", () => {
  it("says how long ago, short", () => {
    expect(ago(10_000, 10_500)).toBe("now");
    expect(ago(0, 42_000)).toBe("42s");
    expect(ago(0, 5 * 60_000)).toBe("5m");
    expect(ago(0, 3 * 3_600_000)).toBe("3h");
    expect(ago(0, 50 * 3_600_000)).toBe("2d");
  });

  it("says how long a turn took", () => {
    expect(took(800)).toBe("<1s");
    expect(took(41_000)).toBe("41s");
    expect(took(192_000)).toBe("3m 12s");
    expect(took(3_720_000)).toBe("1h 2m");
  });
});
