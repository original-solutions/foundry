import { describe, expect, it } from "vitest";
import type { Limits } from "./api";
import type { MachineView } from "./fleet";
import { overLimits, rooms } from "./room";

const limits: Limits = { max_sessions: 4, max_load_per_core: 1.0, min_free_disk: 10_000_000_000 };

function machine(id: string, extra: Partial<MachineView> & { load?: number; sessions?: number; disk?: number } = {}): MachineView {
  const { load = 0.4, sessions = 2, disk = 50_000_000_000, ...rest } = extra;
  return {
    id,
    name: id,
    listed: true,
    status: "online",
    sample: { load_per_core: load, free_memory: 8_000_000_000, free_disk: disk, sessions },
    workspaces: [],
    ...rest,
  };
}

describe("overLimits", () => {
  it("names every limit the sample is at or over, in the order sessions, load, disk, in the daemon's words", () => {
    const sample = { load_per_core: 1.8, free_memory: 0, free_disk: 5_200_000_000, sessions: 4 };
    expect(overLimits(sample, limits)).toEqual(["sessions 4 at limit 4", "load 1.8 over limit 1.0", "free disk 5.2 GB under limit 10.0 GB"]);
  });

  it("names nothing for a sample under every limit", () => {
    expect(overLimits({ load_per_core: 0.5, free_memory: 0, free_disk: 20_000_000_000, sessions: 3 }, limits)).toEqual([]);
  });

  it("prints a value and its limit apart, never as the same number", () => {
    expect(overLimits({ load_per_core: 1.004, free_memory: 0, free_disk: 20e9, sessions: 0 }, limits)).toEqual(["load 1.004 over limit 1.0"]);
    expect(overLimits({ load_per_core: 1, free_memory: 0, free_disk: 20e9, sessions: 0 }, limits)).toEqual(["load 1.0 at limit 1.0"]);
  });
});

describe("rooms", () => {
  it("picks the online machine under its limits with the lowest load, fewer sessions breaking a tie", () => {
    const { pick, room } = rooms([machine("a", { load: 0.6 }), machine("b", { load: 0.3, sessions: 5 }), machine("c", { load: 0.3, sessions: 1 })]);
    expect(pick).toBe("c");
    expect(room.get("c")).toEqual({ kind: "pick" });
    expect(room.get("a")).toEqual({ kind: "room" });
    expect(room.get("b")).toEqual({ kind: "room" });
  });

  it("passes over a machine at or over a limit and an offline one, saying why as the daemon would", () => {
    const { pick, room, reasons } = rooms([
      machine("idle", { load: 0.1, limits, sessions: 4 }),
      machine("away", { load: 0.0, status: "offline" }),
      machine("busy", { load: 0.9 }),
    ]);
    expect(pick).toBe("busy");
    expect(room.get("idle")).toEqual({ kind: "full", over: ["sessions 4 at limit 4"] });
    expect(room.get("away")).toEqual({ kind: "offline" });
    expect(reasons).toEqual(["idle: sessions 4 at limit 4", "away: offline"]);
  });

  it("picks none when no machine qualifies, with one reason per machine that sent numbers", () => {
    const { pick, reasons } = rooms([machine("a", { status: "offline" }), machine("b", { load: 1.2, limits }), machine("c", { sample: null })]);
    expect(pick).toBeUndefined();
    expect(reasons).toEqual(["a: offline", "b: load 1.2 over limit 1.0"]);
  });

  it("decides nothing for a machine with no numbers yet or whose status is not known", () => {
    const { pick, room } = rooms([machine("new", { sample: null }), machine("unsure", { status: "unknown" })]);
    expect(pick).toBeUndefined();
    expect(room.get("new")).toEqual({ kind: "unknown" });
    expect(room.get("unsure")).toEqual({ kind: "unknown" });
  });

  it("breaks a full tie by the lower id, so the pick never depends on the order given", () => {
    expect(rooms([machine("b"), machine("a")]).pick).toBe("a");
  });
});
