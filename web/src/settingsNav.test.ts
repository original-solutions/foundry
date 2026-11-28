import { describe, expect, it } from "vitest";
import type { FactorySettings, MachineSample, Project, ProjectFactory } from "./api";
import type { MachineView } from "./fleet";
import { settingsLine } from "./factory";
import type { Notify } from "./notify";
import { navEntries, type NavEntry, type NavInput } from "./settingsNav";

const factory: FactorySettings = { paused: false, poll_seconds: 30, feedback_seconds: 300, max_runs: 3, max_gates: 2, model: "opus", effort: "high" };
const projects: Project[] = ["a", "b", "c"].map((id) => ({ id, name: id, repo: `https://github.com/o/${id}` }));
const on = (login = "bot"): ProjectFactory => ({ on: true, login, paused: false });
const off: ProjectFactory = { on: false, login: "", paused: false };

const sample = (extra: Partial<MachineSample> = {}): MachineSample => ({ load_per_core: 0.1, free_memory: 1, free_disk: 1, sessions: 0, ...extra });
const machine = (id: string, extra: Partial<MachineView> = {}): MachineView => ({ id, name: id, listed: true, status: "online", sample: sample(), workspaces: [], ...extra });

const input = (extra: Partial<NavInput> = {}): NavInput => ({
  factory,
  projects,
  projectFactory: { a: on(), b: on(), c: off },
  machines: [machine("m1"), machine("m2"), machine("m3")],
  notify: { cards: true, desktop: false, away: true },
  theme: "dark",
  ...extra,
});

const entries = (extra?: Partial<NavInput>) => navEntries(input(extra)).flatMap((g) => g.entries);
const entry = (category: NavEntry["category"], extra?: Partial<NavInput>) => entries(extra).find((e) => e.category === category)!;

describe("navEntries", () => {
  it("groups the categories as Fleet and This browser", () => {
    const groups = navEntries(input());
    expect(groups.map((g) => [g.title, g.entries.map((e) => e.category)])).toEqual([
      ["Fleet", ["factory", "projects", "machines"]],
      ["This browser", ["notifications", "appearance", "keyboard"]],
    ]);
    expect(entries().map((e) => e.label)).toEqual(["Factory", "Projects", "Machines", "Notifications", "Appearance", "Keyboard"]);
  });

  it("summarises Factory as settingsLine, and nothing before the settings are read", () => {
    expect(entry("factory").summary).toBe(settingsLine(factory));
    expect(entry("factory", { factory: undefined }).summary).toBeUndefined();
  });

  it("summarises Projects as how many, and on how many the factory is on", () => {
    expect(entry("projects").summary).toBe("3 projects · factory on 2");
    expect(entry("projects", { projects: projects.slice(0, 1), projectFactory: { a: off } }).summary).toBe("1 project · factory on 0");
  });

  it("summarises Machines as how many, and how many are offline", () => {
    expect(entry("machines", { machines: [machine("m1"), machine("m2"), machine("m3", { status: "offline" })] }).summary).toBe("3 machines · 1 offline");
    expect(entry("machines").summary).toBe("3 machines");
  });

  it("summarises Notifications as the choices that are on", () => {
    expect(entry("notifications").summary).toBe("cards · catch-up");
    const all: Notify = { cards: true, desktop: true, away: true };
    expect(entry("notifications", { notify: all }).summary).toBe("cards · desktop · catch-up");
    expect(entry("notifications", { notify: { cards: false, desktop: false, away: false } }).summary).toBe("all off");
  });

  it("summarises Appearance as the theme, and Keyboard as nothing", () => {
    expect(entry("appearance").summary).toBe("dark");
    expect(entry("appearance", { theme: "light" }).summary).toBe("light");
    expect(entry("keyboard").summary).toBeUndefined();
  });

  it("lists each machine under Machines with the first chip that applies", () => {
    const machines = [
      machine("m1", { name: "personal-mac", acting: true }),
      machine("m2", { name: "studio-mini", sample: sample({ update_ready: "v0.11.2" }) }),
      machine("m3", { name: "build-box", status: "offline" }),
      machine("m4", { name: "plain" }),
    ];
    expect(entry("machines", { machines }).machines).toEqual([
      { id: "m1", name: "personal-mac", chip: "coordinator" },
      { id: "m2", name: "studio-mini", chip: "update ready" },
      { id: "m3", name: "build-box", chip: "offline" },
      { id: "m4", name: "plain" },
    ]);
  });

  it("lets offline win over update ready, and update ready over coordinator", () => {
    const both = machine("m1", { status: "offline", acting: true, sample: sample({ update_ready: "v0.11.2" }) });
    expect(entry("machines", { machines: [both] }).machines?.[0].chip).toBe("offline");
    const ready = machine("m2", { acting: true, sample: sample({ update_ready: "v0.11.2" }) });
    expect(entry("machines", { machines: [ready] }).machines?.[0].chip).toBe("update ready");
  });

  it("lists machines only under Machines", () => {
    expect(entries().filter((e) => e.machines !== undefined).map((e) => e.category)).toEqual(["machines"]);
  });
});
