import type { AnyRecord, MachineSample, SessionState, SharedSettings, TerminalState, UiMessage } from "./api";

export interface FleetState {
  records: AnyRecord[];
  settings: SharedSettings;
  /** True once a full state has arrived, so a missing record means it is not there. */
  synced: boolean;
}

export const emptyState: FleetState = {
  records: [],
  settings: { seq: 0, machines: [], priority: [], projects: [], limits: {} },
  synced: false,
};

/** A machine sample is one per owner; every other record is one per kind and body id. */
const key = (record: AnyRecord) => (record.kind === "machine_sample" ? `sample:${record.owner}` : `${record.kind}:${record.body.id}`);

/** Folds one message from the events socket into the state. */
export function apply(state: FleetState, message: UiMessage): FleetState {
  switch (message.type) {
    case "state":
      return { records: message.records, settings: message.settings, synced: true };
    case "settings":
      return { ...state, settings: message.settings };
    case "record": {
      const next = message.record;
      const at = state.records.findIndex((r) => key(r) === key(next));
      if (at < 0) return { ...state, records: [...state.records, next] };
      const old = state.records[at];
      // Mirror Record::supersedes: the same owner, a higher version.
      if (old.owner !== next.owner || next.version <= old.version) return state;
      return { ...state, records: state.records.map((r, i) => (i === at ? next : r)) };
    }
  }
}

/**
 * Whether terminal `id` is listed as running and not deleted, or `undefined` before the first
 * full state, when nothing is known yet.
 */
export function terminalRunning(state: FleetState, id: string): boolean | undefined {
  if (!state.synced) return undefined;
  return state.records.some((r) => r.kind === "terminal" && r.body.id === id && !r.deleted && r.body.state === "running");
}

export interface SessionView {
  id: string;
  model: string;
  state: SessionState;
  terminal: string;
}
export interface ShellView {
  id: string;
  state: TerminalState;
  exitCode: number | null;
}
export interface WorkspaceView {
  id: string;
  name: string;
  branch: string;
  project: string;
  sessions: SessionView[];
  shells: ShellView[];
}
export interface MachineView {
  id: string;
  name: string;
  status: "online" | "offline" | "unknown";
  sample: MachineSample | null;
  workspaces: WorkspaceView[];
}

/** The sidebar's model. Machine rules follow `foundryd status`. */
export function fleet(records: AnyRecord[], settings: SharedSettings): MachineView[] {
  const listed = settings.machines.map((m) => m.id);
  const others = [...new Set(records.filter((r) => r.kind === "machine_sample" && !listed.includes(r.owner)).map((r) => r.owner))].sort();
  return [...listed, ...others].map((id) => {
    const reachable = records.flatMap((r) =>
      r.owner === id && (r.kind === "session" || r.kind === "terminal") ? [r.body.state !== "unreachable"] : [],
    );
    let sample: { version: number; body: MachineSample } | null = null;
    for (const r of records) {
      if (r.kind === "machine_sample" && r.owner === id && (!sample || r.version > sample.version)) sample = r;
    }

    const live = records.filter((r) => !r.deleted);
    const sessions = live.flatMap((r) => (r.kind === "session" ? [r.body] : []));
    const referenced = new Set(records.flatMap((r) => (r.kind === "session" ? [r.body.terminal] : [])));
    const workspaces = live.flatMap((r) => {
      if (r.kind !== "workspace" || r.owner !== id) return [];
      const w = r.body;
      const mine = sessions.filter((s) => s.workspace === w.id);
      return [
        {
          id: w.id,
          name: w.name,
          branch: w.branch,
          project: settings.projects.find((p) => p.id === w.project)?.name ?? w.project,
          sessions: mine.map((s) => ({ id: s.id, model: s.model, state: s.state, terminal: s.terminal })),
          shells: live.flatMap((t) =>
            t.kind === "terminal" && t.body.workspace === w.id && !referenced.has(t.body.id)
              ? [{ id: t.body.id, state: t.body.state, exitCode: t.body.exit_code }]
              : [],
          ),
        },
      ];
    });
    return {
      id,
      name: settings.machines.find((m) => m.id === id)?.name ?? id,
      status: reachable.includes(false) ? "offline" : reachable.length === 0 ? "unknown" : "online",
      sample: sample?.body ?? null,
      workspaces,
    };
  });
}

/** The numbers line of `foundryd status`. */
export function machineNumbers(sample: MachineSample | null): string {
  if (!sample) return "no sample yet";
  const gb = (bytes: number) => (bytes / 1e9).toFixed(1);
  return `load ${sample.load_per_core.toFixed(2)}/core  memory ${gb(sample.free_memory)} GB free  disk ${gb(sample.free_disk)} GB free  ${sample.sessions} session${sample.sessions === 1 ? "" : "s"}`;
}
