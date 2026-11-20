import { describe, expect, it } from "vitest";
import type { Mark } from "./api";
import { ago, kindOf, tally, took, turns } from "./trail";

const mark = (kind: Mark["kind"], text: string, at: number): Mark => ({ kind, text, at });
const tool = (name: string, text: string): Mark => ({ kind: "tool", tool: name, text, at: 0 });

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
  it("names what a tool step did from the tool the hook named", () => {
    expect(kindOf(tool("Bash", "$ ls"))).toBe("command");
    expect(kindOf(tool("Edit", "Edit README.md"))).toBe("edit");
    expect(kindOf(tool("Write", "Write a.ts"))).toBe("edit");
    expect(kindOf(tool("MultiEdit", "MultiEdit a.ts"))).toBe("edit");
    expect(kindOf(tool("NotebookEdit", "NotebookEdit a.ipynb"))).toBe("edit");
    expect(kindOf(tool("Read", "Read a.ts"))).toBe("read");
    expect(kindOf(tool("Grep", "Grep fn main"))).toBe("search");
    expect(kindOf(tool("Glob", "Glob **/*.md"))).toBe("search");
    expect(kindOf(tool("WebFetch", "Fetch https://x.dev"))).toBe("web");
    expect(kindOf(tool("WebSearch", "Search rust traits"))).toBe("web");
    expect(kindOf(tool("Task", "Agent: find it"))).toBe("agent");
    expect(kindOf(tool("Agent", "Agent: find it"))).toBe("agent");
    expect(kindOf(tool("TodoWrite", "TodoWrite"))).toBe("other");
    expect(kindOf(tool("mcp__github__get_me", "mcp__github__get_me"))).toBe("other");
    expect(kindOf(mark("ask", "Claude needs you", 0))).toBe("ask");
  });

  it("goes by the tool, never by what the display text happens to start with", () => {
    expect(kindOf(tool("Read", "$ not a command"))).toBe("read");
    expect(kindOf(tool("mcp__x__Edit", "Edit a.ts"))).toBe("other");
    expect(kindOf(mark("tool", "$ ls", 0))).toBe("other");
    expect(kindOf(tool("constructor", "constructor"))).toBe("other");
  });
});

describe("tally", () => {
  it("counts a turn's steps by what they did, biggest first, in plain words", () => {
    const steps = [tool("Bash", "$ ls"), tool("Edit", "Edit a"), tool("Bash", "$ npm test"), tool("Read", "Read b"), mark("ask", "ok?", 0)];
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
