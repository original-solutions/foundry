import type { AnyRecord, SessionState, SharedSettings, TerminalState } from "./api";

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
  extra: { model?: string; deleted?: boolean; version?: number } = {},
): AnyRecord {
  return {
    kind: "session",
    owner,
    version: extra.version ?? 1,
    deleted: extra.deleted ?? false,
    body: { id, workspace: workspaceId, terminal: terminalId, model: extra.model ?? "opus", state },
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
