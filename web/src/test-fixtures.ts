import type { AnyRecord, FactoryStatus, HeldUp, SessionState, SharedSettings, TerminalState } from "./api";

export const ME = "machine-1";
export const OTHER = "machine-2";

export function workspace(owner: string, id: string, name: string, extra: { project?: string; deleted?: boolean; version?: number } = {}): AnyRecord {
  return {
    kind: "workspace",
    owner,
    version: extra.version ?? 1,
    deleted: extra.deleted ?? false,
    body: { id, project: extra.project ?? "p1", name, branch: name.replace(/ /g, "-"), base: "main" },
  };
}

export function terminal(
  owner: string,
  id: string,
  workspaceId: string,
  state: TerminalState = "running",
  extra: { exit_code?: number | null; deleted?: boolean; version?: number } = {},
): AnyRecord {
  return {
    kind: "terminal",
    owner,
    version: extra.version ?? 1,
    deleted: extra.deleted ?? false,
    body: { id, workspace: workspaceId, state, exit_code: extra.exit_code ?? null },
  };
}

export function session(
  owner: string,
  id: string,
  workspaceId: string,
  terminalId: string,
  state: SessionState = "idle",
  extra: { model?: string; deleted?: boolean; version?: number; name?: string } = {},
): AnyRecord {
  return {
    kind: "session",
    owner,
    version: extra.version ?? 1,
    deleted: extra.deleted ?? false,
    body: { id, workspace: workspaceId, terminal: terminalId, model: extra.model ?? "opus", state, ...(extra.name === undefined ? {} : { name: extra.name }) },
  };
}

export function sample(owner: string, version: number, extra: { load?: number; mem?: number; disk?: number; sessions?: number } = {}): AnyRecord {
  return {
    kind: "machine_sample",
    owner,
    version,
    deleted: false,
    body: {
      load_per_core: extra.load ?? 0.42,
      free_memory: extra.mem ?? 12_300_000_000,
      free_disk: extra.disk ?? 120_500_000_000,
      sessions: extra.sessions ?? 4,
    },
  };
}

export function settings(extra: Partial<SharedSettings> = {}): SharedSettings {
  return {
    seq: 1,
    machines: [{ id: ME, name: "studio", address: "127.0.0.1" }],
    priority: [ME],
    projects: [{ id: "p1", name: "demo", repo: "/tmp/demo.git" }],
    limits: {},
    ...extra,
  };
}

/** A workspace record that sits on a commit, with no branch. */
export function onCommit(owner: string, id: string, name: string): AnyRecord {
  const record = workspace(owner, id, name);
  if (record.kind === "workspace") record.body = { ...record.body, branch: "", base: "fedcba9876543210fedcba9876543210fedcba98" };
  return record;
}

/** A believed factory status of `project`: its live runs by id with their workspaces, and its held-up runs. */
export function factoryStatus(
  project: string,
  live: Record<string, string[]>,
  held: Record<string, { kind: HeldUp["kind"]; workspaces?: string[]; reason?: string }> = {},
): Record<string, FactoryStatus> {
  return {
    [project]: {
      project,
      pass: 1,
      holds: true,
      generation: 1,
      not_holding: null,
      idle: null,
      owner_ignored: false,
      ready: [],
      feedback_waiting: [],
      runs: Object.entries(live).map(([run, workspaces]) => ({ run, title: "t", step: "building", machine: ME, gate_retries_used: 0, minutes_used: 3, workspaces, sessions: [] })),
      held_up: Object.entries(held).map(([run, h]) => ({ run, kind: h.kind, reason: h.reason ?? "why", ...(h.workspaces && { workspaces: h.workspaces }) })),
      gate_infra: [],
      finished: [],
      foreign: [],
    },
  };
}

/** Project p1's status: issue 162's run lives in w1, and issue 170's run is held up (`held`) in w2. */
export function factoryStatuses(held: HeldUp["kind"] = "stuck"): Record<string, FactoryStatus> {
  return factoryStatus("p1", { "original-solutions__foundry__162__1": ["w1"] }, { "original-solutions__foundry__170__1": { kind: held, workspaces: ["w2"] } });
}
