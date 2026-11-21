import { describe, expect, it } from "vitest";
import type { AnyRecord, UiMessage } from "./api";
import { apply, byUrgency, emptyState, fleet, locate, machineNumbers, machineStats, rows, terminalRunning } from "./fleet";
import type { Glyph } from "./labels";
import { ME, OTHER, sample, session, settings, terminal, workspace } from "./test-fixtures";

const full = (records: AnyRecord[]): UiMessage => ({ type: "state", protocol: 1, records, settings: settings() });
const one = (record: AnyRecord): UiMessage => ({ type: "record", protocol: 1, record });

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

  it("replaces the settings on a settings message", () => {
    const next = settings({ seq: 2, projects: [] });
    const state = apply(emptyState, { type: "settings", protocol: 1, settings: next });
    expect(state.settings).toEqual(next);
  });
});

describe("fleet", () => {
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

  it("leaves a session with no brief, as a peer's is, as it was", () => {
    const [machine] = fleet(records, settings());
    expect(rows(machine.workspaces[0]).every((r) => r.title === undefined && r.now === undefined)).toBe(true);
  });
});
