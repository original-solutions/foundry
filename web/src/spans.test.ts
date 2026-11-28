import { describe, expect, it } from "vitest";
import type { Mark } from "./api";
import { bars, heading, layers, placed, RANGES, sum, ticks, waits, type Bar } from "./spans";

const m = (kind: Mark["kind"], text: string, at: number): Mark => ({ kind, text, at });
const MIN = 60_000;

describe("bars", () => {
  it("draws each turn from its prompt to its end, with the tools and questions it had", () => {
    const trail = [m("prompt", "fix it", 1000), m("tool", "$ ls", 2000), m("ask", "Proceed?", 3000), m("done", "", 5000), m("prompt", "again", 9000), m("done", "", 9500)];
    const all = bars(trail, false, 20_000);
    expect(all.map((b) => [b.start, b.end, b.open, b.prompt])).toEqual([
      [1000, 5000, false, "fix it"],
      [9000, 9500, false, "again"],
    ]);
    expect(all[0].steps.map((s) => s.text)).toEqual(["$ ls", "Proceed?"]);
  });

  it("marks a reminder that arrives after a turn ended as a nudge, not a turn", () => {
    const trail = [m("prompt", "hi", 1000), m("done", "", 2000), m("ask", "Claude is waiting for your input", 60_000)];
    const [turn, nudge] = bars(trail, false, 70_000);
    expect(turn.nudge).toBe(false);
    expect(nudge).toMatchObject({ start: 60_000, end: 60_000, nudge: true });
    expect(bars([m("ask", "Proceed?", 1000), m("tool", "$ ls", 2000)], true, 3000)[0].nudge).toBe(false);
  });

  it("runs the newest unfinished turn up to now while the session is busy", () => {
    const trail = [m("prompt", "go", 1000), m("tool", "$ ls", 2000)];
    expect(bars(trail, true, 8000)[0]).toMatchObject({ start: 1000, end: 8000, open: true });
  });

  it("ends an unfinished turn at its last step when the session is no longer busy", () => {
    const trail = [m("prompt", "go", 1000), m("tool", "$ ls", 2000), m("prompt", "next", 4000), m("done", "", 6000)];
    const [cut, next] = bars(trail, false, 8000);
    expect(cut).toMatchObject({ start: 1000, end: 2000, open: false });
    expect(next).toMatchObject({ start: 4000, end: 6000, open: false });
    expect(bars([m("prompt", "go", 1000)], false, 8000)[0]).toMatchObject({ start: 1000, end: 1000, open: false });
  });
});

describe("placed", () => {
  const from = 0;
  const to = 100 * MIN;
  it("places a bar as a share of the window, cut to it", () => {
    expect(placed({ start: 10 * MIN, end: 30 * MIN }, from, to)).toEqual({ left: 10, width: 20 });
    expect(placed({ start: -50 * MIN, end: 5 * MIN }, from, to)).toEqual({ left: 0, width: 5 });
    expect(placed({ start: 90 * MIN, end: 140 * MIN }, from, to)).toEqual({ left: 90, width: 10 });
  });

  it("leaves out a bar wholly outside the window", () => {
    expect(placed({ start: -50 * MIN, end: -1 }, from, to)).toBeUndefined();
    expect(placed({ start: to + 1, end: to + 5 }, from, to)).toBeUndefined();
  });
});

describe("ticks", () => {
  it("marks the window at even steps back from now, newest last", () => {
    expect(ticks(RANGES[1].ms).map((t) => [t.left, t.label])).toEqual([
      [25, "45m"],
      [50, "30m"],
      [75, "15m"],
    ]);
    expect(ticks(RANGES[3].ms).map((t) => t.label)).toEqual(["9h", "6h", "3h"]);
    expect(ticks(RANGES[0].ms).map((t) => t.label)).toEqual(["10m", "5m"]);
  });
});

describe("sum", () => {
  it("counts the turns, agent time, tools and questions inside the window", () => {
    const bar = (start: number, end: number, steps: Mark[] = [], nudge = false): Bar => ({ start, end, open: false, nudge, steps });
    const all = [
      bar(0, 10 * MIN, [m("tool", "$ old", 5 * MIN)]),
      bar(-30 * MIN, -20 * MIN, [m("tool", "$ gone", -25 * MIN)]),
      bar(20 * MIN, 22 * MIN, [m("tool", "$ ls", 21 * MIN), m("ask", "Proceed?", 21.5 * MIN)]),
      bar(30 * MIN, 30 * MIN, [m("ask", "Claude is waiting for your input", 30 * MIN)], true),
    ];
    expect(sum([all, [bar(50 * MIN, 51 * MIN)]], -5 * MIN, 60 * MIN)).toEqual({ turns: 3, busy: 13 * MIN, waited: 0, tools: 2, asks: 1 });
  });

  it("takes the time a turn stood waiting on you out of agent work, inside the window", () => {
    const bar: Bar = { start: 0, end: 30 * MIN, open: true, nudge: false, steps: [] };
    const waited = [{ start: 4 * MIN, end: 10 * MIN, open: false }, { start: 15 * MIN, end: 30 * MIN, open: true }];
    // 23 minutes of the turn fall in the window; 6 + 10 of them it stood waiting.
    expect(sum([[bar]], 2 * MIN, 25 * MIN, [waited])).toMatchObject({ busy: 7 * MIN, waited: 16 * MIN });
  });
});

describe("waits", () => {
  it("runs a question to the step that came after it: the time the turn stood waiting on you", () => {
    const trail = [m("prompt", "fix it", 0), m("tool", "$ ls", 1 * MIN), m("ask", "Proceed?", 2 * MIN), m("tool", "$ rm", 9 * MIN), m("ask", "Push?", 10 * MIN), m("done", "", 12 * MIN)];
    const [bar] = bars(trail, false, 20 * MIN);
    expect(waits(bar, false, 20 * MIN)).toEqual([
      { start: 2 * MIN, end: 9 * MIN, open: false },
      { start: 10 * MIN, end: 12 * MIN, open: false },
    ]);
  });

  it("runs a question still unanswered up to now while the session waits on you", () => {
    const trail = [m("prompt", "fix it", 0), m("tool", "$ ls", 1 * MIN), m("ask", "Proceed?", 2 * MIN)];
    const [bar] = bars(trail, true, 20 * MIN);
    expect(waits(bar, true, 20 * MIN)).toEqual([{ start: 2 * MIN, end: 20 * MIN, open: true }]);
    // Answered, with no step since: when it was answered is not known yet, so nothing is claimed.
    expect(waits(bar, false, 20 * MIN)).toEqual([]);
  });

  it("dates a wait the trail holds no question for from the turn's last step", () => {
    const [bar] = bars([m("prompt", "fix it", 0), m("tool", "$ ls", 3 * MIN)], true, 20 * MIN);
    expect(waits(bar, true, 20 * MIN)).toEqual([{ start: 3 * MIN, end: 20 * MIN, open: true }]);
  });

  it("finds no wait in a reminder between turns", () => {
    const [, nudge] = bars([m("prompt", "hi", 0), m("done", "", 1 * MIN), m("ask", "Claude is waiting for your input", 5 * MIN)], false, 20 * MIN);
    expect(waits(nudge, false, 20 * MIN)).toEqual([]);
  });
});

describe("layers", () => {
  it("cuts the waits of every session into spans by how many stood waiting at once, inside the window", () => {
    const w = (start: number, end: number) => ({ start: start * MIN, end: end * MIN, open: false });
    expect(layers([[w(0, 10)], [w(5, 15), w(30, 40)], [w(8, 12)]], 2 * MIN, 35 * MIN)).toEqual([
      { start: 2 * MIN, end: 5 * MIN, depth: 1 },
      { start: 5 * MIN, end: 8 * MIN, depth: 2 },
      { start: 8 * MIN, end: 10 * MIN, depth: 3 },
      { start: 10 * MIN, end: 12 * MIN, depth: 2 },
      { start: 12 * MIN, end: 15 * MIN, depth: 1 },
      { start: 30 * MIN, end: 35 * MIN, depth: 1 },
    ]);
    expect(layers([], 0, 10 * MIN)).toEqual([]);
  });
});

describe("heading", () => {
  it("quotes a prompt you sent, and names what Claude Code sent itself", () => {
    expect(heading({ start: 0, end: 0, open: false, nudge: false, steps: [], prompt: "fix it" })).toBe("“fix it”");
    expect(heading({ start: 0, end: 0, open: false, nudge: false, steps: [], prompt: "<task-notification> <task-id>b1</task-id>" })).toBe("A background task finished");
    expect(heading({ start: 0, end: 0, open: false, nudge: false, steps: [], prompt: "<local-command-stdout>ok</local-command-stdout>" })).toBe("A message from Claude Code");
    expect(heading({ start: 0, end: 0, open: false, nudge: false, steps: [] })).toBe("Earlier steps");
  });
});
