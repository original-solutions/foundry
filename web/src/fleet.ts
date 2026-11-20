import type { AnyRecord, Briefs, MachineSample, SessionState, SharedSettings, Step, TerminalState, UiMessage } from "./api";
import { busy, plural, sessionLabel, sessionGlyph, terminalLabel, terminalGlyph, urgency, type Glyph } from "./labels";

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
  /** Its first prompt, when this machine's daemon has its brief. */
  title?: string;
  /** The newest step of its turn. */
  now?: Step;
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

/** The sidebar's model. Machine rules follow `foundryd status`. `briefs` name sessions by what they were asked. */
export function fleet(records: AnyRecord[], settings: SharedSettings, briefs: Briefs = {}): MachineView[] {
  const listed = settings.machines.map((m) => m.id);
  const others = [...new Set(records.filter((r) => r.kind === "machine_sample" && !listed.includes(r.owner)).map((r) => r.owner))].sort();
  const live = records.filter((r) => !r.deleted);
  const sessions = live.flatMap((r) => (r.kind === "session" ? [r.body] : []));
  const referenced = new Set(sessions.map((s) => s.terminal));
  const shellsOf = new Map<string, ShellView[]>();
  for (const t of live) {
    if (t.kind !== "terminal" || referenced.has(t.body.id)) continue;
    const shells = shellsOf.get(t.body.workspace) ?? [];
    shells.push({ id: t.body.id, state: t.body.state, exitCode: t.body.exit_code });
    shellsOf.set(t.body.workspace, shells);
  }
  return [...listed, ...others].map((id) => {
    const reachable = records.flatMap((r) =>
      r.owner === id && (r.kind === "session" || r.kind === "terminal") ? [r.body.state !== "unreachable"] : [],
    );
    let sample: { version: number; body: MachineSample } | null = null;
    for (const r of records) {
      if (r.kind === "machine_sample" && r.owner === id && (!sample || r.version > sample.version)) sample = r;
    }

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
          sessions: mine.map((s) => ({
            id: s.id,
            model: s.model,
            state: s.state,
            terminal: s.terminal,
            title: briefs[s.id]?.title ?? undefined,
            now: briefs[s.id]?.now ?? undefined,
          })),
          shells: shellsOf.get(w.id) ?? [],
        },
      ];
    });
    return {
      id,
      name: settings.machines.find((m) => m.id === id)?.name ?? shortId(id),
      status: reachable.includes(false) ? "offline" : reachable.length === 0 ? "unknown" : "online",
      sample: sample?.body ?? null,
      workspaces,
    };
  });
}

/** Where terminal `id` sits in the fleet, with the name and state the sidebar gives it. */
export interface Located {
  machine: MachineView;
  workspace: WorkspaceView;
  /** The session it runs, when it is a session's terminal. */
  session?: string;
  name: string;
  /** A session's first prompt. */
  title?: string;
  label: string;
  glyph: Glyph;
}

export function locate(machines: MachineView[], id: string): Located | undefined {
  for (const machine of machines) {
    for (const workspace of machine.workspaces) {
      const row = rows(workspace).find((r) => r.terminal === id);
      if (row) return { machine, workspace, session: row.session, name: row.name, title: row.title, label: row.label, glyph: row.glyph };
    }
  }
  return undefined;
}

/** One terminal of a workspace as the sidebar lists it. */
export interface Row {
  terminal: string;
  /** The session it runs, when it is a session's terminal. */
  session?: string;
  name: string;
  /** A session's first prompt, which tells two sessions of one model apart. */
  title?: string;
  /** What a session is doing, only while it is `busy`: in the middle of a turn. */
  now?: Step;
  label: string;
  glyph: Glyph;
}

/** A workspace's terminals in sidebar order: its sessions, then its shells. */
export function rows(workspace: WorkspaceView): Row[] {
  return [
    ...workspace.sessions.map((s) => {
      const glyph = sessionGlyph[s.state];
      return { terminal: s.terminal, session: s.id, name: s.model, title: s.title, now: busy.includes(glyph) ? s.now : undefined, label: sessionLabel[s.state], glyph };
    }),
    ...workspace.shells.map((t) => ({ terminal: t.id, name: "shell", label: terminalLabel(t.state, t.exitCode), glyph: terminalGlyph(t.state, t.exitCode) })),
  ];
}

/** `rows` with what needs a person first, then what is busy, then the rest; ties keep their order. */
export function byUrgency(list: Row[]): Row[] {
  return [...list].sort((a, b) => urgency.indexOf(a.glyph) - urgency.indexOf(b.glyph));
}

/** A machine id to show when settings give no name: a UUID's first block, else the id. */
function shortId(id: string): string {
  return /^[0-9a-f]{8}-[0-9a-f]{4}-/i.test(id) ? id.slice(0, 8) : id;
}

/** The numbers line of `foundryd status`. */
export function machineNumbers(sample: MachineSample | null): string {
  if (!sample) return "no sample yet";
  const gb = (bytes: number) => (bytes / 1e9).toFixed(1);
  return `load ${sample.load_per_core.toFixed(2)}/core  memory ${gb(sample.free_memory)} GB free  disk ${gb(sample.free_disk)} GB free  ${plural(sample.sessions, "session")}`;
}

/** The same numbers, short enough for one sidebar line. */
export function machineStats(sample: MachineSample | null): string {
  if (!sample) return "no sample yet";
  const gb = (bytes: number) => (bytes / 1e9).toFixed(1);
  return `${sample.load_per_core.toFixed(2)}/core · ${gb(sample.free_memory)} GB mem · ${gb(sample.free_disk)} GB disk · ${plural(sample.sessions, "session")}`;
}
