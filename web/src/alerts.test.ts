import { describe, expect, it } from "vitest";
import type { AnyRecord, SessionState } from "./api";
import { alertsFor, favicon, pulse, rippleFrames, tabTitle } from "./alerts";
import { fleet } from "./fleet";
import { shapes } from "./StateGlyph";
import { ME, sample, session, settings, terminal, workspace } from "./test-fixtures";

/** One workspace "tidy" with one opus session in `state`, plus whatever `more` adds. */
function world(state: SessionState, more: AnyRecord[] = []) {
  return fleet([sample(ME, 1), workspace(ME, "w1", "tidy"), terminal(ME, "t1", "w1"), session(ME, "s1", "w1", "t1", state), ...more], settings());
}

describe("alertsFor", () => {
  it("raises nothing on first sight, and remembers every session's state", () => {
    const { alerts, seen } = alertsFor(undefined, world("waiting_for_you"));
    expect(alerts).toEqual([]);
    expect(seen.get("s1")).toBe("waiting_for_you");
  });

  it("says a session finished its turn when it goes from working to idle", () => {
    const { seen } = alertsFor(undefined, world("working"));
    const { alerts } = alertsFor(seen, world("idle"));
    expect(alerts).toEqual([{ terminal: "t1", tag: "s1", glyph: "ready", title: "tidy", body: "opus finished its turn" }]);
  });

  it("stays quiet when a session goes idle from anything but working, or keeps its state", () => {
    for (const from of ["starting", "idle", "unreachable"] as const) {
      const { seen } = alertsFor(undefined, world(from));
      expect(alertsFor(seen, world("idle")).alerts).toEqual([]);
    }
    const { seen } = alertsFor(undefined, world("working"));
    expect(alertsFor(seen, world("working")).alerts).toEqual([]);
  });

  it("raises waiting, stuck and failed each time a session enters them", () => {
    const cases = [
      ["waiting_for_you", "waiting", "opus is waiting for you"],
      ["stuck", "stuck", "opus is stuck: no output for 10 minutes"],
      ["failed", "failed", "opus failed"],
    ] as const;
    for (const [state, glyph, body] of cases) {
      const { seen } = alertsFor(undefined, world("working"));
      expect(alertsFor(seen, world(state)).alerts).toEqual([{ terminal: "t1", tag: "s1", glyph, title: "tidy", body }]);
    }
  });

  it("raises a session that shows up already needing you, but not one that shows up quiet", () => {
    const { seen } = alertsFor(undefined, world("idle"));
    const later = world("idle", [terminal(ME, "t2", "w1"), session(ME, "s2", "w1", "t2", "waiting_for_you", { model: "sonnet" }), terminal(ME, "t3", "w1"), session(ME, "s3", "w1", "t3", "idle")]);
    expect(alertsFor(seen, later).alerts.map((a) => a.body)).toEqual(["sonnet is waiting for you"]);
  });
});

describe("pulse", () => {
  it("counts what needs you and what works, shells included, and leads with the most urgent glyph", () => {
    const machines = world("working", [
      workspace(ME, "w2", "other"),
      terminal(ME, "t2", "w2"),
      session(ME, "s2", "w2", "t2", "working"),
      terminal(ME, "t3", "w2", "exited", { exit_code: 1 }),
    ]);
    expect(pulse(machines)).toEqual({ glyph: "failed", needYou: 1, working: 2 });
  });

  it("has no glyph for an empty fleet", () => {
    expect(pulse(fleet([], settings()))).toEqual({ glyph: undefined, needYou: 0, working: 0 });
  });
});

describe("tabTitle", () => {
  it("leads with what needs you, then what works, else just the name", () => {
    expect(tabTitle({ glyph: "waiting", needYou: 2, working: 3 })).toBe("(2) Foundry · 2 need you");
    expect(tabTitle({ glyph: "waiting", needYou: 1, working: 0 })).toBe("(1) Foundry · 1 needs you");
    expect(tabTitle({ glyph: "working", needYou: 0, working: 3 })).toBe("Foundry · 3 working");
    expect(tabTitle({ glyph: "ready", needYou: 0, working: 0 })).toBe("Foundry");
  });
});

describe("favicon", () => {
  const decode = (url: string) => decodeURIComponent(url.replace("data:image/svg+xml,", ""));
  const lit = (svg: string) => [...svg.matchAll(/<circle[^>]*opacity="1"/g)].length;

  it("draws the glyph's shape in its colour on a dark tile", () => {
    const svg = decode(favicon("waiting"));
    expect(svg).toContain("<rect");
    expect(svg).toContain("#fbbf24");
    expect(svg.match(/<circle/g)).toHaveLength(9);
    expect(lit(svg)).toBe([...shapes.waiting].filter((c) => c === "#").length);
  });

  it("draws a given ripple frame in place of the still shape", () => {
    const svg = decode(favicon("working", rippleFrames[0]));
    expect(lit(svg)).toBe([...rippleFrames[0]].filter((c) => c === "#").length);
    expect(svg).toContain("#38bdf8");
  });

  it("draws a resting session grey, so the tab icon has colour only when something works or needs you", () => {
    const svg = decode(favicon("ready"));
    expect(svg).not.toContain("#34d399");
    expect(svg).toContain("#a3a3a3");
  });

  it("draws a grey grid when there is nothing to show", () => {
    expect(decode(favicon(undefined))).toContain("#737373");
  });
});
