import { describe, expect, it } from "vitest";
import type { AnyRecord, FactoryStatus, UiMessage } from "./api";
import { apply, branchOf, byUrgency, coordinatorOrder, emptyState, factoryRuns, fleet, locate, machineNumbers, machineStats, rows, runStage, terminalRunning, withFactory } from "./fleet";
import type { Glyph } from "./labels";
import { factoryStatus, ME, OTHER, sample, session, settings, terminal, workspace } from "./test-fixtures";

const full = (records: AnyRecord[]): UiMessage => ({ type: "state", protocol: 1, records, settings: settings() });
const one = (record: AnyRecord): UiMessage => ({ type: "record", protocol: 1, record });

const status = (project: string, generation: number): FactoryStatus => ({
  project,
  pass: 1,
  holds: true,
  generation,
  not_holding: null,
  idle: null,
  owner_ignored: false,
  ready: [],
  feedback_waiting: [],
  runs: [],
  held_up: [],
  gate_infra: [],
  finished: [],
  foreign: [],
});
const said = (s: FactoryStatus): UiMessage => ({ type: "factory_status", protocol: 5, status: s });

describe("apply and the factory", () => {
  it("keeps the last status the socket sent for each project, and a later one replaces only its own project's", () => {
    let state = apply(emptyState, said(status("p1", 1)));
    state = apply(state, said(status("p2", 4)));
    state = apply(state, said(status("p1", 2)));
    expect(state.factory?.p1.generation).toBe(2);
    expect(state.factory?.p2.generation).toBe(4);
  });

  it("drops the held statuses on a state message, which the daemon follows with each still believed", () => {
    let state = apply(emptyState, said(status("p1", 1)));
    state = apply(state, full([workspace(ME, "w1", "a")]));
    expect(state.factory).toEqual({});
    state = apply(state, said(status("p1", 3)));
    expect(state.factory?.p1.generation).toBe(3);
  });

  it("keeps the statuses through a record and a settings message", () => {
    let state = apply(emptyState, said(status("p1", 1)));
    state = apply(state, one(workspace(ME, "w1", "a")));
    state = apply(state, { type: "settings", protocol: 1, settings: settings({ seq: 9 }) });
    expect(state.factory?.p1.generation).toBe(1);
    expect(state.settings.seq).toBe(9);
  });

  it("keeps the very same statuses object through a record and a settings message", () => {
    const first = apply(emptyState, said(status("p1", 1)));
    const before = first.factory;
    const appended = apply(first, one(workspace(ME, "w1", "a", { version: 1 })));
    const replaced = apply(appended, one(workspace(ME, "w1", "b", { version: 2 })));
    const moved = apply(replaced, { type: "settings", protocol: 1, settings: settings({ seq: 9 }) });
    expect(appended.factory).toBe(before);
    expect(replaced.factory).toBe(before);
    expect(moved.factory).toBe(before);
  });

  it("returns the very same state for a message type it does not know", () => {
    const state = apply(apply(emptyState, full([workspace(ME, "w1", "a")])), said(status("p1", 1)));
    const next = apply(state, { type: "something_new", protocol: 6 } as unknown as UiMessage);
    expect(next).toBe(state);
    expect(next.records).toHaveLength(1);
    expect(next.settings.machines[0].name).toBe("studio");
  });
});

describe("apply", () => {
  it("replaces everything on a state message", () => {
    const first = apply(emptyState, full([workspace(ME, "w1", "a")]));
    const second = apply(first, full([workspace(ME, "w2", "b")]));
    expect(second.records.map((r) => (r.kind === "workspace" ? r.body.id : ""))).toEqual(["w2"]);
    expect(second.settings.machines[0].name).toBe("studio");
  });

  it("adds a record and replaces it only with a higher version from the same owner", () => {
    let state = apply(emptyState, one(workspace(ME, "w1", "old", { version: 2 })));
    state = apply(state, one(workspace(ME, "w1", "stale", { version: 2 })));
    state = apply(state, one(workspace(ME, "w1", "older", { version: 1 })));
    state = apply(state, one(workspace(OTHER, "w1", "intruder", { version: 9 })));
    expect(state.records).toHaveLength(1);
    expect(state.records[0].body).toMatchObject({ name: "old" });
    state = apply(state, one(workspace(ME, "w1", "new", { version: 3 })));
    expect(state.records).toHaveLength(1);
    expect(state.records[0].body).toMatchObject({ name: "new" });
  });

  it("keys a machine sample by its owner, not by a body id", () => {
    let state = apply(emptyState, one(sample(ME, 1)));
    state = apply(state, one(sample(OTHER, 1)));
    state = apply(state, one(sample(ME, 2, { sessions: 9 })));
    expect(state.records).toHaveLength(2);
    const mine = state.records.find((r) => r.owner === ME);
    expect(mine?.version).toBe(2);
  });

  it("keeps a deleted record so a stale copy cannot bring it back", () => {
    let state = apply(emptyState, one(workspace(ME, "w1", "a", { version: 1 })));
    state = apply(state, one(workspace(ME, "w1", "a", { version: 2, deleted: true })));
    expect(state.records[0].deleted).toBe(true);
  });

  it("takes the online set from a state message and keeps it across record and settings messages", () => {
    let state = apply(emptyState, { type: "state", protocol: 1, records: [], settings: settings(), online: [ME] });
    expect(state.online).toEqual([ME]);
    state = apply(state, one(sample(OTHER, 1)));
    state = apply(state, { type: "settings", protocol: 1, settings: settings({ seq: 2 }) });
    expect(state.online).toEqual([ME]);
    state = apply(state, full([]));
    expect(state.online).toBeUndefined();
  });

  it("replaces the settings on a settings message", () => {
    const next = settings({ seq: 2, projects: [] });
    const state = apply(emptyState, { type: "settings", protocol: 1, settings: next });
    expect(state.settings).toEqual(next);
  });
});

describe("fleet", () => {
  describe("a machine the machine list does not hold", () => {
    const records = [
      sample(ME, 1),
      workspace(ME, "w1", "mine"),
      terminal(ME, "t1", "w1"),
      session(ME, "s1", "w1", "t1"),
      terminal(ME, "sh1", "w1"),
      sample(OTHER, 1),
      workspace(OTHER, "w2", "gone"),
      terminal(OTHER, "t2", "w2"),
      session(OTHER, "s2", "w2", "t2"),
      terminal(OTHER, "sh2", "w2"),
    ];

    it("has no workspaces, sessions or shells once the list holds a machine, and keeps its card", () => {
      const machines = fleet(records, settings({ machines: [{ id: ME, name: "studio", address: "" }] }));
      expect(machines.map((m) => m.id)).toEqual([ME, OTHER]);
      const [mine, gone] = machines;
      expect(mine.workspaces.map((w) => w.name)).toEqual(["mine"]);
      expect(mine.workspaces[0].sessions.map((x) => x.id)).toEqual(["s1"]);
      expect(mine.workspaces[0].shells).toHaveLength(1);
      expect(gone.listed).toBe(false);
      expect(gone.sample).not.toBeNull();
      expect(gone.workspaces).toEqual([]);
    });

    it("hides nothing while the list is empty", () => {
      const machines = fleet(records, settings({ machines: [] }));
      expect(machines.flatMap((m) => m.workspaces.map((w) => w.name)).sort()).toEqual(["gone", "mine"]);
    });
  });

  describe("the serving machine, when the machine list does not hold it", () => {
    const X = "machine-x";
    // The list holds OTHER alone: ME, the machine serving the page, was removed or has not reached it yet.
    const listed = settings({ machines: [{ id: OTHER, name: "laptop", address: "" }] });
    const mine = [workspace(ME, "w1", "mine"), terminal(ME, "t1", "w1"), session(ME, "s1", "w1", "t1"), terminal(ME, "sh1", "w1")];
    const records = [
      sample(ME, 1),
      ...mine,
      sample(X, 1),
      workspace(X, "w2", "gone"),
      terminal(X, "t2", "w2"),
      session(X, "s2", "w2", "t2"),
      terminal(X, "sh2", "w2"),
    ];
    const card = (machines: ReturnType<typeof fleet>, id: string) => machines.find((m) => m.id === id);

    it("lists its own workspaces, sessions and terminals under its card", () => {
      const here = card(fleet(records, listed, {}, undefined, ME), ME);
      expect(here?.listed).toBe(false);
      expect(here?.workspaces.map((w) => w.name)).toEqual(["mine"]);
      expect(here?.workspaces[0].sessions.map((s) => [s.id, s.terminal])).toEqual([["s1", "t1"]]);
      expect(here?.workspaces[0].shells.map((t) => t.id)).toEqual(["sh1"]);
    });

    it("still lists none for another machine the list does not hold", () => {
      const away = card(fleet(records, listed, {}, undefined, ME), X);
      expect(away?.sample).not.toBeNull();
      expect(away?.workspaces).toEqual([]);
    });

    it("keeps its card and its records before its own first sample", () => {
      const here = card(fleet([...mine, sample(X, 1)], listed, {}, undefined, ME), ME);
      expect(here?.sample).toBeNull();
      expect(here?.workspaces.map((w) => w.name)).toEqual(["mine"]);
    });

    it("hides it as any other machine while the serving machine is not known", () => {
      const machines = fleet(records, listed, {}, undefined, undefined);
      expect(machines.map((m) => [m.id, m.workspaces.length])).toEqual([
        [OTHER, 0],
        [ME, 0],
        [X, 0],
      ]);
      // Before its first sample it has no card at all.
      expect(card(fleet(mine, listed), ME)).toBeUndefined();
    });

    it("gets no card of its own from workspaces it has deleted alone", () => {
      const deleted = [workspace(ME, "w1", "mine", { deleted: true })];
      expect(card(fleet(deleted, listed, {}, undefined, ME), ME)).toBeUndefined();
    });
  });

  it("lists machines in settings order, then others that have a sample, named by id", () => {
    const s = settings({
      machines: [
        { id: "b", name: "second", address: "" },
        { id: "a", name: "first", address: "" },
      ],
    });
    const machines = fleet([sample("z", 1), sample("a", 1), sample("c", 1)], s);
    expect(machines.map((m) => m.name)).toEqual(["second", "first", "c", "z"]);
  });

  it("names a machine settings do not name by the start of its id when the id is a UUID", () => {
    const id = "e2b2e8f1-52f2-4dd0-8575-7c6e2b6745d7";
    expect(fleet([sample(id, 1)], settings({ machines: [] })).map((m) => m.name)).toEqual(["e2b2e8f1"]);
  });

  it("is offline when any session or terminal of the machine is unreachable, online when one is not, else unknown", () => {
    const s = settings({
      machines: [
        { id: "m1", name: "one", address: "" },
        { id: "m2", name: "two", address: "" },
        { id: "m3", name: "three", address: "" },
      ],
    });
    const records = [
      workspace("m1", "w1", "a"),
      terminal("m1", "t1", "w1", "running"),
      session("m1", "s1", "w1", "t1", "unreachable"),
      workspace("m2", "w2", "b"),
      terminal("m2", "t2", "w2", "running"),
    ];
    const state = Object.fromEntries(fleet(records, s).map((m) => [m.name, m.status]));
    expect(state).toEqual({ one: "offline", two: "online", three: "unknown" });
  });

  it("with the online set, is online when the set holds the machine and offline when not, before any terminal", () => {
    const s = settings({
      machines: [
        { id: "m1", name: "one", address: "" },
        { id: "m2", name: "two", address: "" },
        { id: "m3", name: "three", address: "" },
      ],
    });
    const records = [sample("m1", 1), sample("m2", 1), sample("m3", 1)];
    const state = Object.fromEntries(fleet(records, s, {}, ["m1"]).map((m) => [m.name, m.status]));
    expect(state).toEqual({ one: "online", two: "offline", three: "offline" });
  });

  it("with the online set, records do not decide: a running terminal of a machine it lacks is offline", () => {
    const s = settings({
      machines: [
        { id: "m1", name: "one", address: "" },
        { id: "m2", name: "two", address: "" },
      ],
    });
    const records = [
      workspace("m1", "w1", "a"),
      terminal("m1", "t1", "w1", "running"),
      workspace("m2", "w2", "b"),
      terminal("m2", "t2", "w2", "unreachable"),
      sample("m9", 1),
      sample("m8", 1),
    ];
    const state = Object.fromEntries(fleet(records, s, {}, ["m2", "m9"]).map((m) => [m.name, m.status]));
    expect(state).toEqual({ one: "offline", two: "online", m8: "offline", m9: "online" });
  });

  it("stays offline when a reachable record of the machine comes after an unreachable one", () => {
    const s = settings({ machines: [{ id: "m1", name: "one", address: "" }] });
    const records = [
      workspace("m1", "w1", "a"),
      session("m1", "s1", "w1", "t1", "unreachable"),
      terminal("m1", "t1", "w1", "running"),
    ];
    expect(fleet(records, s).map((m) => m.status)).toEqual(["offline"]);
  });

  it("uses the newest sample by version", () => {
    const machines = fleet([sample(ME, 5, { sessions: 5 }), sample(ME, 7, { sessions: 7 }), sample(ME, 6, { sessions: 6 })], settings());
    expect(machines[0].sample?.sessions).toBe(7);
  });

  it("groups workspaces, sessions and shells, hiding what is deleted", () => {
    const records = [
      sample(ME, 1),
      workspace(ME, "w1", "fix login", { project: "p1" }),
      workspace(ME, "w2", "gone", { deleted: true }),
      terminal(ME, "t1", "w1", "running"),
      session(ME, "s1", "w1", "t1", "idle"),
      terminal(ME, "t2", "w1", "exited", { exit_code: 1 }),
      terminal(ME, "t3", "w1", "running", { deleted: true }),
      session(ME, "s2", "w1", "t9", "working", { deleted: true }),
      terminal(ME, "t4", "w2", "running"),
      session(ME, "s3", "w2", "t4", "idle"),
    ];
    const [machine] = fleet(records, settings());
    expect(machine.workspaces.map((w) => w.name)).toEqual(["fix login"]);
    const [w] = machine.workspaces;
    expect(w).toMatchObject({ id: "w1", branch: "fix-login", project: "demo" });
    expect(w.sessions).toEqual([{ id: "s1", model: "opus", name: "opus", state: "idle", terminal: "t1" }]);
    expect(w.shells).toEqual([{ id: "t2", name: "shell", state: "exited", exitCode: 1 }]);
  });

  it("hides a session whose terminal was closed, which the daemon leaves failed behind it", () => {
    const records = [
      sample(ME, 1),
      workspace(ME, "w1", "a"),
      terminal(ME, "t1", "w1", "exited", { deleted: true, version: 2 }),
      session(ME, "s1", "w1", "t1", "failed"),
      session(ME, "s2", "w1", "t2", "starting"),
    ];
    const [w] = fleet(records, settings())[0].workspaces;
    expect(w.sessions.map((s) => s.id)).toEqual(["s2"]);
    expect(w.shells).toEqual([]);
  });

  it("lists a running terminal whose session was deleted as a shell", () => {
    const records = [
      sample(ME, 1),
      workspace(ME, "w1", "a"),
      terminal(ME, "t1", "w1", "running"),
      session(ME, "s1", "w1", "t1", "idle", { deleted: true }),
    ];
    const [w] = fleet(records, settings())[0].workspaces;
    expect(w.sessions).toEqual([]);
    expect(w.shells).toEqual([{ id: "t1", name: "shell", state: "running", exitCode: null }]);
  });

  it("lists a workspace's sessions and shells in the order their terminals opened, so a reload, which sends records by version, does not reshuffle them", () => {
    const records = [
      sample(ME, 1),
      workspace(ME, "w1", "a"),
      terminal(ME, "t2", "w1", "running", { version: 5 }),
      terminal(ME, "sh2", "w1", "running", { version: 7 }),
      terminal(ME, "t1", "w1", "running", { version: 2 }),
      session(ME, "s2", "w1", "t2", "idle", { version: 6 }),
      terminal(ME, "sh1", "w1", "running", { version: 3 }),
      // s1 changed state last, so a fresh state sends it last; its terminal opened first.
      session(ME, "s1", "w1", "t1", "working", { version: 9 }),
    ];
    const [w] = fleet(records, settings())[0].workspaces;
    expect(w.sessions.map((s) => s.id)).toEqual(["s1", "s2"]);
    expect(w.shells.map((t) => t.id)).toEqual(["sh1", "sh2"]);
  });

  it("names sessions of one model and shells apart by number in opening order, and leaves a lone one plain", () => {
    const records = [
      sample(ME, 1),
      workspace(ME, "w1", "a"),
      terminal(ME, "t1", "w1", "running", { version: 1 }),
      session(ME, "s1", "w1", "t1", "idle", { version: 9 }),
      terminal(ME, "t2", "w1", "running", { version: 2 }),
      session(ME, "s2", "w1", "t2", "idle", { model: "sonnet" }),
      terminal(ME, "t3", "w1", "running", { version: 3 }),
      session(ME, "s3", "w1", "t3", "working"),
      terminal(ME, "sh1", "w1", "running", { version: 4 }),
      terminal(ME, "sh2", "w1", "exited", { version: 8, exit_code: 0 }),
      workspace(ME, "w2", "b"),
      terminal(ME, "t4", "w2", "running"),
      session(ME, "s4", "w2", "t4", "idle"),
      terminal(ME, "sh3", "w2", "running"),
    ];
    const [one, two] = fleet(records, settings())[0].workspaces;
    expect(rows(one).map((r) => r.name)).toEqual(["opus 1", "sonnet", "opus 2", "shell 1", "shell 2"]);
    expect(rows(two).map((r) => r.name)).toEqual(["opus", "shell"]);
    expect(locate(fleet(records, settings()), "t3")?.name).toBe("opus 2");
  });

  it("shows the project id when settings do not name the project", () => {
    const [machine] = fleet([sample(ME, 1), workspace(ME, "w1", "a", { project: "lost" })], settings());
    expect(machine.workspaces[0].project).toBe("lost");
  });
});

describe("machineNumbers", () => {
  it("formats like foundryd status", () => {
    expect(machineNumbers({ load_per_core: 0.42, free_memory: 12_300_000_000, free_disk: 120_500_000_000, sessions: 4 })).toBe(
      "load 0.42/core  memory 12.3 GB free  disk 120.5 GB free  4 sessions",
    );
    expect(machineNumbers({ load_per_core: 1, free_memory: 0, free_disk: 0, sessions: 1 })).toContain("1 session");
    expect(machineNumbers({ load_per_core: 1, free_memory: 0, free_disk: 0, sessions: 1 })).not.toContain("1 sessions");
    expect(machineNumbers(null)).toBe("no sample yet");
  });
});

describe("machineStats", () => {
  it("gives the sidebar's short form of the same numbers", () => {
    expect(machineStats({ load_per_core: 0.42, free_memory: 12_300_000_000, free_disk: 120_500_000_000, sessions: 4 })).toBe(
      "0.42/core · 12.3 GB mem · 120.5 GB disk · 4 sessions",
    );
    expect(machineStats({ load_per_core: 1, free_memory: 0, free_disk: 0, sessions: 1 })).toMatch(/· 1 session$/);
    expect(machineStats(null)).toBe("no sample yet");
  });
});

describe("terminalRunning", () => {
  it("does not know before the first state arrives", () => {
    expect(terminalRunning(emptyState, "t1")).toBeUndefined();
    expect(terminalRunning(apply(emptyState, one(terminal(ME, "t1", "w1"))), "t1")).toBeUndefined();
  });

  it("is true only for a listed terminal that runs and is not deleted", () => {
    const state = apply(
      emptyState,
      full([
        terminal(ME, "run", "w1", "running"),
        terminal(ME, "done", "w1", "exited", { exit_code: 0 }),
        terminal(ME, "gone", "w1", "running", { deleted: true }),
        terminal(OTHER, "away", "w1", "unreachable"),
      ]),
    );
    expect(terminalRunning(state, "run")).toBe(true);
    expect(terminalRunning(state, "done")).toBe(false);
    expect(terminalRunning(state, "gone")).toBe(false);
    expect(terminalRunning(state, "away")).toBe(false);
    expect(terminalRunning(state, "never")).toBe(false);
  });

  it("replaces an existing record without mutating the prior state array", () => {
    const initial = apply(emptyState, full([sample(ME, 1), workspace(ME, "w1", "a")]));
    const prevRecords = initial.records;
    const updated = apply(initial, one(sample(ME, 2, { load: 2.5 })));
    expect(prevRecords[0].version).toBe(1);
    expect(updated.records[0].version).toBe(2);
    expect(updated.records).toHaveLength(2);
  });
});

describe("locate", () => {
  const machines = fleet(
    [
      sample(ME, 1),
      workspace(ME, "w1", "fix login"),
      terminal(ME, "t1", "w1", "running"),
      session(ME, "s1", "w1", "t1", "waiting_for_you", { model: "sonnet" }),
      terminal(ME, "sh1", "w1", "exited", { exit_code: 2 }),
      workspace(OTHER, "w2", "docs"),
      terminal(OTHER, "sh2", "w2", "unreachable"),
    ],
    settings({ machines: [{ id: ME, name: "studio", address: "" }, { id: OTHER, name: "laptop", address: "" }] }),
  );

  it("finds a session's terminal with its workspace, machine, name and state", () => {
    const found = locate(machines, "t1");
    expect(found && { machine: found.machine.name, workspace: found.workspace.name, name: found.name, label: found.label, glyph: found.glyph }).toEqual({
      machine: "studio",
      workspace: "fix login",
      name: "sonnet",
      label: "Needs you",
      glyph: "waiting",
    });
  });

  it("finds a shell, named shell, with its exit code in the label", () => {
    const found = locate(machines, "sh1");
    expect(found && [found.name, found.label, found.glyph]).toEqual(["shell", "Failed · exit 2", "failed"]);
    const away = locate(machines, "sh2");
    expect(away && [away.machine.name, away.workspace.name, away.label, away.glyph]).toEqual(["laptop", "docs", "Unreachable", "away"]);
  });

  it("lists a workspace's terminals as rows, sessions first, each with its session, name, label and glyph", () => {
    expect(rows(machines[0].workspaces[0])).toEqual([
      { terminal: "t1", session: "s1", name: "sonnet", label: "Needs you", glyph: "waiting" },
      { terminal: "sh1", name: "shell", label: "Failed · exit 2", glyph: "failed" },
    ]);
  });

  it("orders rows by what needs a person first: waiting, stuck, failed, working, then the rest", () => {
    const row = (terminal: string, glyph: Glyph) => ({ terminal, name: terminal, label: "", glyph });
    const all = ["done", "ready", "working", "away", "failed", "starting", "stuck", "waiting"].map((g) => row(g, g as Glyph));
    expect(byUrgency(all).map((r) => r.glyph)).toEqual(["waiting", "stuck", "failed", "working", "starting", "ready", "done", "away"]);
  });

  it("finds nothing for a terminal that is not listed", () => {
    expect(locate(machines, "nope")).toBeUndefined();
  });
});

describe("briefs", () => {
  const records = [
    workspace(ME, "w1", "fix login"),
    terminal(ME, "t1", "w1"),
    session(ME, "s1", "w1", "t1", "working"),
    terminal(ME, "t2", "w1"),
    session(ME, "s2", "w1", "t2", "idle"),
    terminal(ME, "t3", "w1"),
    session(ME, "s3", "w1", "t3", "waiting_for_you"),
  ];
  const briefs = {
    s1: { title: "fix the login bug", now: { kind: "tool" as const, text: "Edit src/login.ts" } },
    s2: { title: "add a test", now: { kind: "prompt" as const, text: "left over" } },
    s3: { title: null, now: { kind: "ask" as const, text: "Claude needs your permission to use Bash" } },
  };

  it("gives each session its first prompt as a title, so two sessions of one model read apart", () => {
    const [machine] = fleet(records, settings(), briefs);
    const list = rows(machine.workspaces[0]);
    expect(list.map((r) => [r.name, r.title])).toEqual([
      ["opus 1", "fix the login bug"],
      ["opus 2", "add a test"],
      ["opus 3", undefined],
    ]);
    expect(locate([machine], "t1")?.title).toBe("fix the login bug");
  });

  it("says what a session is doing only while it is in a turn: starting, working, waiting or stuck", () => {
    const [machine] = fleet(records, settings(), briefs);
    const list = rows(machine.workspaces[0]);
    expect(list.map((r) => r.now?.text)).toEqual(["Edit src/login.ts", undefined, "Claude needs your permission to use Bash"]);
    const starting = [workspace(ME, "w1", "fix login"), terminal(ME, "t1", "w1"), session(ME, "s1", "w1", "t1", "starting")];
    const [early] = fleet(starting, settings(), { s1: { title: "fix the login bug", now: { kind: "prompt" as const, text: "fix the login bug" } } });
    expect(rows(early.workspaces[0])[0].now?.text).toBe("fix the login bug");
  });

  it("keeps what Claude said at the end of a turn only for a session that is idle or finished", () => {
    const said = "All done.";
    const finished = [...records, terminal(ME, "t4", "w1"), session(ME, "s4", "w1", "t4", "finished"), terminal(ME, "t5", "w1"), session(ME, "s5", "w1", "t5", "failed")];
    const all = Object.fromEntries(["s1", "s2", "s3", "s4", "s5"].map((id) => [id, { title: null, now: null, said }]));
    const [machine] = fleet(finished, settings(), all);
    expect(rows(machine.workspaces[0]).map((r) => r.said)).toEqual([undefined, said, undefined, said, undefined]);
  });

  it("shows the name a session was given in place of its first prompt, a peer's session too, and the first prompt once it has none", () => {
    const named = [
      ...records.map((r) => (r.kind === "session" && r.body.id === "s1" ? session(ME, "s1", "w1", "t1", "working", { name: "login redirect" }) : r)),
      terminal(OTHER, "t9", "w9"),
      workspace(OTHER, "w9", "far"),
      session(OTHER, "s9", "w9", "t9", "idle", { name: "far work" }),
    ];
    const listed = settings({ machines: [{ id: ME, name: "studio", address: "" }, { id: OTHER, name: "attic", address: "" }] });
    const view = fleet(named, listed, briefs);
    const mine = view.find((m) => m.id === ME)!;
    expect(rows(mine.workspaces[0]).map((r) => r.title)).toEqual(["login redirect", "add a test", undefined]);
    expect(locate(view, "t1")?.title).toBe("login redirect");
    const theirs = view.find((m) => m.id === OTHER)!;
    expect(rows(theirs.workspaces[0])[0].title).toBe("far work");
  });

  it("leaves a session with no brief, as a peer's is, as it was", () => {
    const [machine] = fleet(records, settings());
    expect(rows(machine.workspaces[0]).every((r) => r.title === undefined && r.now === undefined)).toBe(true);
  });
});

describe("the coordinator order", () => {
  const A = "00000000-0000-4000-8000-00000000000a";
  const B = "00000000-0000-4000-8000-00000000000b";
  const C = "00000000-0000-4000-8000-00000000000c";
  const named = (...ids: string[]) => ids.map((id) => ({ id, name: `m-${id.slice(-1)}`, address: "" }));
  const view = (machines: string[], priority: string[], online?: string[]) =>
    fleet([], settings({ machines: named(...machines), priority }), {}, online);
  const rank = (views: ReturnType<typeof view>) => Object.fromEntries(views.map((m) => [m.id.slice(-1), m.rank]));

  it("orders the listed machines by the priority, then the rest by id, and ranks no unlisted machine", () => {
    expect(coordinatorOrder([C], [A, B, C])).toEqual([C, A, B]);
    expect(coordinatorOrder([], [B, A])).toEqual([A, B]);
    // An id the machine list does not hold is skipped, and an id listed twice counts once.
    expect(coordinatorOrder(["gone", B, B], [A, B, C])).toEqual([B, A, C]);
  });

  it("gives each listed machine its place, with a partial priority and an unlisted id in it", () => {
    expect(rank(view([A, B, C], [C, "gone", B]))).toEqual({ a: 2, b: 1, c: 0 });
    const unlisted = fleet([sampleOf("stranger")], settings({ machines: named(A), priority: [A] }));
    expect(unlisted.find((m) => m.id === "stranger")?.rank).toBeUndefined();
  });

  it("marks the first machine of the priority that is online as acting", () => {
    const views = view([A, B, C], [A, B, C], [B, C]);
    expect(views.filter((m) => m.acting).map((m) => m.id)).toEqual([B]);
  });

  it("marks the lowest online id when the priority names none that is online", () => {
    expect(view([A, B, C], [], [C, B]).find((m) => m.acting)?.id).toBe(B);
    expect(view([A, B, C], [A], [C, B]).find((m) => m.acting)?.id).toBe(B);
  });

  it("marks the first of the priority when nothing is online, and nothing when it is empty too", () => {
    expect(view([A, B], [B, A], []).find((m) => m.acting)?.id).toBe(B);
    expect(view([A, B], [], []).some((m) => m.acting)).toBe(false);
  });

  it("marks no machine when the state did not say which are online", () => {
    expect(view([A, B], [A, B]).some((m) => m.acting)).toBe(false);
  });
});

function sampleOf(owner: string): AnyRecord {
  return sample(owner, 1);
}

describe("a workspace's base, project and branch", () => {
  const commitOnly = (): AnyRecord => {
    const record = workspace(ME, "w1", "review a commit");
    if (record.kind === "workspace") record.body = { ...record.body, branch: "", base: "0123456789abcdef0123456789abcdef01234567" };
    return record;
  };

  it("carries its record's base and its project's id beside the project's name", () => {
    const [machine] = fleet([sample(ME, 1), workspace(ME, "w1", "fix login")], settings());
    expect(machine.workspaces[0]).toMatchObject({ project: "demo", projectId: "p1", base: "main" });
  });

  it("shows its branch, or the first 7 characters of its commit when its branch is empty", () => {
    const [machine] = fleet([sample(ME, 1), workspace(ME, "w2", "fix login"), commitOnly()], settings());
    expect(machine.workspaces.map(branchOf)).toEqual(["fix-login", "0123456"]);
  });
});

describe("a factory run's stage", () => {
  const run = "o__r__1027__2";
  const records = (): AnyRecord[] => [
    sample(ME, 1),
    workspace(ME, "w1", "fac/1027-2"),
    terminal(ME, "t1", "w1"),
    session(ME, "s1", "w1", "t1", "finished"),
    terminal(ME, "t2", "w1"),
    session(ME, "s2", "w1", "t2", "working"),
    terminal(ME, "t3", "w1"),
    session(ME, "s3", "w1", "t3", "waiting_for_you"),
    terminal(ME, "t4", "w1"),
    session(ME, "s4", "w1", "t4", "idle"),
    terminal(ME, "sh1", "w1", "exited", { exit_code: 0 }),
    terminal(ME, "sh2", "w1", "exited", { exit_code: 2 }),
    terminal(ME, "sh3", "w1"),
  ];
  const workspaceIn = (statuses: Record<string, FactoryStatus>) => withFactory(fleet(records(), settings()), statuses)[0].workspaces[0];
  const labels = (statuses: Record<string, FactoryStatus>) => rows(workspaceIn(statuses)).map((r) => [r.terminal, r.label]);

  it("says what a stuck run is stuck on, cut short, on each terminal at rest, with the whole reason beside it", () => {
    const reason = "gate red: make check fails on main, see the gate log for the failing target";
    const statuses = factoryStatus("p1", {}, { [run]: { kind: "stuck", workspaces: ["w1"], reason } });
    const short = "Stuck · gate red: make check…";
    expect(labels(statuses)).toEqual([
      ["t1", short],
      ["t2", "Working"],
      ["t3", "Needs you"],
      ["t4", short],
      ["sh1", short],
      ["sh2", short],
      ["sh3", short],
    ]);
    expect(rows(workspaceIn(statuses))[0].fullLabel).toBe(`Stuck · ${reason}`);
    expect(rows(workspaceIn(statuses))[1].fullLabel).toBeUndefined();
  });

  it("puts nothing beside a label that already says the whole reason", () => {
    const statuses = factoryStatus("p1", {}, { [run]: { kind: "stuck", workspaces: ["w1"], reason: "make check fails on main" } });
    const [ended] = rows(workspaceIn(statuses));
    expect([ended.label, ended.fullLabel]).toEqual(["Stuck · make check fails on main", undefined]);
  });

  it("says needs info with the reason's first line, and a run held up with no reason by its kind alone", () => {
    const info = factoryStatus("p1", {}, { [run]: { kind: "needs_info", workspaces: ["w1"], reason: "Which API version?\nThe issue names two." } });
    expect(runStage(factoryRuns(info)(workspaceIn(info))!)).toEqual({ label: "Needs info · Which API version?", full: "Needs info · Which API version?\nThe issue names two." });
    expect(runStage({ run, held: "stuck", reason: " " })).toEqual({ label: "Stuck", full: "Stuck" });
  });

  it("says a live run's step in words, with the gate's failed tries while it builds", () => {
    expect(runStage({ run, step: "building" })?.label).toBe("Building");
    expect(runStage({ run, step: "building", gateRetries: 2 })?.label).toBe("Building · gate retry 2");
    expect(runStage({ run, step: "pr-open" })?.label).toBe("PR open");
    expect(runStage({ run, step: "claimed" })?.label).toBe("Claimed");
    expect(runStage({ run, step: "triage" })?.label).toBe("Triage");
    // A step a newer daemon sends is said as it is named.
    expect(runStage({ run, step: "merge-queue" })?.label).toBe("Merge queue");
    expect(runStage({ run })).toBeUndefined();
  });

  it("carries a live run's step and gate retries from its status, and names its stage on its terminals that ended", () => {
    const statuses = factoryStatus("p1", { [run]: ["w1"] });
    statuses.p1.runs[0].gate_retries_used = 1;
    expect(workspaceIn(statuses).factory).toMatchObject({ step: "building", gateRetries: 1 });
    const building = "Building · gate retry 1";
    // A ready session or an open shell keeps its own state: the factory moves it, and its turn ending is news.
    expect(labels(statuses).map(([, label]) => label)).toEqual([building, "Working", "Needs you", "Ready", building, building, "Open"]);
  });

  it("leaves a workspace no run tends with its terminals' own labels", () => {
    expect(rows(fleet(records(), settings())[0].workspaces[0]).map((r) => r.label)).toEqual(["Ended", "Working", "Needs you", "Ready", "Ended", "Failed · exit 2", "Open"]);
  });
});
