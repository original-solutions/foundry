import type { AnyRecord, FactoryStatus, HeldUp, Briefs, Limits, MachineSample, SessionState, SharedSettings, Step, TerminalState, UiMessage } from "./api";
import { issueUrl, parseRun } from "./factory";
import { busy, plural, sessionLabel, sessionGlyph, terminalLabel, terminalGlyph, urgency, type Glyph } from "./labels";

export interface FleetState {
  records: AnyRecord[];
  settings: SharedSettings;
  /** True once a full state has arrived, so a missing record means it is not there. */
  synced: boolean;
  /** Every machine the daemon serving the page hears, itself included, when its state carries it. */
  online?: string[];
  /** The last status the socket sent for each project's factory, by project id. */
  factory?: Record<string, FactoryStatus>;
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
      // The daemon sends the whole state again when a project's status is no longer believed, then
      // each status still believed, so what was held is dropped here.
      return { records: message.records, settings: message.settings, synced: true, online: message.online, factory: {} };
    case "factory_status":
      return { ...state, factory: { ...state.factory, [message.status.project]: message.status } };
    case "settings":
      return { ...state, settings: message.settings };
    case "record": {
      const next = message.record;
      const nextKey = key(next);
      const at = state.records.findIndex((r) => key(r) === nextKey);
      if (at < 0) return { ...state, records: [...state.records, next] };
      const old = state.records[at];
      // Mirror Record::supersedes: the same owner, a higher version.
      if (old.owner !== next.owner || next.version <= old.version) return state;
      const records = state.records.slice();
      records[at] = next;
      return { ...state, records };
    }
    default:
      // A message type this page does not know is left out, so a newer daemon cannot break it.
      return state;
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
  /** What every screen calls it: its model, numbered when its workspace has another of that model. */
  name: string;
  state: SessionState;
  terminal: string;
  /** Its first prompt, when this machine's daemon has its brief. */
  title?: string;
  /** The newest step of its turn. */
  now?: Step;
  /** What Claude said when its last turn ended. */
  said?: string;
}
export interface ShellView {
  id: string;
  /** "shell", numbered when its workspace has more than one. */
  name: string;
  state: TerminalState;
  exitCode: number | null;
}
export interface WorkspaceView {
  id: string;
  name: string;
  /** Empty for a workspace on a commit, whose commit is its `base`. */
  branch: string;
  /** The branch or commit it started from. */
  base: string;
  /** Its project's name. */
  project: string;
  projectId: string;
  sessions: SessionView[];
  shells: ShellView[];
  /** The factory run it belongs to, once `withFactory` has read the believed statuses. */
  factory?: FactoryRun;
}
export interface MachineView {
  id: string;
  name: string;
  /** Whether the machine list holds it: only a listed machine can be renamed. */
  listed: boolean;
  /** The address the machine list holds for it; none for a machine it does not list. */
  address?: string;
  /** The limits the settings hold for it, if any. */
  limits?: Limits;
  /** Its place (from 0) in the coordinator order, for a listed machine only. */
  rank?: number;
  /** Whether it holds the coordinator role; only marked when the state said which machines are online. */
  acting?: boolean;
  status: "online" | "offline" | "unknown";
  sample: MachineSample | null;
  workspaces: WorkspaceView[];
}

/**
 * The listed machines in the order they take the coordinator role: the ids of `priority` that
 * `listed` holds, each once, then the other listed machines by id.
 */
export function coordinatorOrder(priority: string[], listed: string[]): string[] {
  const held = new Set(listed);
  const first = priority.filter((id, i) => held.has(id) && priority.indexOf(id) === i);
  const rest = listed.filter((id) => !first.includes(id)).sort();
  return [...first, ...rest];
}

/** The machine that holds the coordinator role, as `coordinator::acting` picks it. */
function actingMachine(priority: string[], online: string[]): string | undefined {
  return priority.find((id) => online.includes(id)) ?? [...online].sort()[0] ?? priority[0];
}

/**
 * The model the sidebar, the board and the machines board share. Machine rules follow `foundryd
 * status`. `briefs` name sessions by what they were asked. With `online`, the machines the daemon
 * hears, a machine is online when the set holds it and offline when not, whatever its records say.
 * Without it, a machine is offline when one of its records is unreachable, online when one is not,
 * else unknown. Once the machine list holds a machine, a machine it does not hold (a removed one)
 * has no workspaces, so none of its workspaces, terminals or sessions show anywhere; its card
 * stays, named by id with its last sample. Its records stay in the state. While the list is empty
 * (a lone machine that has not listed itself yet) nothing is hidden. The exception is `here`, the
 * machine serving the page: its own records are always listed, under a card of its own, whatever
 * the list holds, so a removed machine's UI still reaches its own sessions.
 */
export function fleet(records: AnyRecord[], settings: SharedSettings, briefs: Briefs = {}, online?: string[], here?: string): MachineView[] {
  const machineNames = new Map<string, string>();
  const machineAddresses = new Map<string, string>();
  const listed: string[] = [];
  for (const m of settings.machines) {
    listed.push(m.id);
    machineNames.set(m.id, m.name);
    machineAddresses.set(m.id, m.address);
  }
  const projectNames = new Map<string, string>();
  for (const p of settings.projects) projectNames.set(p.id, p.name);

  const otherSet = new Set<string>();
  const samplesByOwner = new Map<string, { version: number; body: MachineSample }>();
  const statusByOwner = new Map<string, "online" | "offline">();
  // A session whose terminal was closed has ended for good: its record stays, failed, behind it.
  const closed = new Set<string>();
  // A fresh state sends records by version, so a session that just changed comes last. A
  // running terminal's record is written when it opens and again only when it exits, so its
  // version keeps the order terminals opened in across reloads.
  const opened = new Map<string, number>();
  const liveSessions: { id: string; workspace: string; model: string; state: SessionState; terminal: string }[] = [];
  const liveTerminals: { workspace: string; view: Omit<ShellView, "name"> }[] = [];
  const workspacesByOwner = new Map<string, { id: string; name: string; branch: string; base: string; project: string; projectId: string }[]>();

  for (const r of records) {
    switch (r.kind) {
      case "machine_sample": {
        if (!machineNames.has(r.owner)) otherSet.add(r.owner);
        const prev = samplesByOwner.get(r.owner);
        if (!prev || r.version > prev.version) samplesByOwner.set(r.owner, r);
        break;
      }
      case "session": {
        if (r.body.state === "unreachable") statusByOwner.set(r.owner, "offline");
        else if (!statusByOwner.has(r.owner)) statusByOwner.set(r.owner, "online");
        if (!r.deleted) liveSessions.push(r.body);
        break;
      }
      case "terminal": {
        if (r.body.state === "unreachable") statusByOwner.set(r.owner, "offline");
        else if (!statusByOwner.has(r.owner)) statusByOwner.set(r.owner, "online");
        if (r.deleted) {
          closed.add(r.body.id);
        } else {
          opened.set(r.body.id, r.version);
          liveTerminals.push({
            workspace: r.body.workspace,
            view: { id: r.body.id, state: r.body.state, exitCode: r.body.exit_code },
          });
        }
        break;
      }
      case "workspace": {
        if (!r.deleted) {
          if (r.owner === here && !machineNames.has(here)) otherSet.add(here);
          let list = workspacesByOwner.get(r.owner);
          if (!list) {
            list = [];
            workspacesByOwner.set(r.owner, list);
          }
          list.push({
            id: r.body.id,
            name: r.body.name,
            branch: r.body.branch,
            base: r.body.base,
            project: projectNames.get(r.body.project) ?? r.body.project,
            projectId: r.body.project,
          });
        }
        break;
      }
    }
  }

  const referenced = new Set<string>();
  const sessionsByWorkspace = new Map<string, typeof liveSessions>();
  for (const s of liveSessions) {
    if (closed.has(s.terminal)) continue;
    referenced.add(s.terminal);
    let list = sessionsByWorkspace.get(s.workspace);
    if (!list) {
      list = [];
      sessionsByWorkspace.set(s.workspace, list);
    }
    list.push(s);
  }

  const shellsOf = new Map<string, Omit<ShellView, "name">[]>();
  for (const t of liveTerminals) {
    if (referenced.has(t.view.id)) continue;
    let shells = shellsOf.get(t.workspace);
    if (!shells) {
      shells = [];
      shellsOf.set(t.workspace, shells);
    }
    shells.push(t.view);
  }

  const byOpening = <T>(list: T[], terminal: (t: T) => string) =>
    [...list].sort((a, b) => (opened.get(terminal(a)) ?? Infinity) - (opened.get(terminal(b)) ?? Infinity));

  const others = [...otherSet].sort();
  const ranks = new Map(coordinatorOrder(settings.priority, listed).map((id, i) => [id, i]));
  const acting = online ? actingMachine(settings.priority, online) : undefined;
  return [...listed, ...others].map((id) => ({
    id,
    name: machineNames.get(id) ?? shortId(id),
    listed: machineNames.has(id),
    address: machineAddresses.get(id),
    limits: settings.limits[id],
    rank: ranks.get(id),
    acting: acting === undefined ? undefined : id === acting,
    status: online ? (online.includes(id) ? "online" : "offline") : (statusByOwner.get(id) ?? "unknown"),
    sample: samplesByOwner.get(id)?.body ?? null,
    workspaces: (settings.machines.length > 0 && !machineNames.has(id) && id !== here ? [] : (workspacesByOwner.get(id) ?? [])).map((w) => {
      const mine = byOpening(sessionsByWorkspace.get(w.id) ?? [], (s) => s.terminal);
      const sessionNames = numbered(mine.map((s) => s.model));
      const shells = byOpening(shellsOf.get(w.id) ?? [], (t) => t.id);
      const shellNames = numbered(shells.map(() => "shell"));
      return {
        ...w,
        sessions: mine.map((s, i) => ({
          id: s.id,
          model: s.model,
          name: sessionNames[i],
          state: s.state,
          terminal: s.terminal,
          title: briefs[s.id]?.title ?? undefined,
          now: briefs[s.id]?.now ?? undefined,
          said: briefs[s.id]?.said,
        })),
        shells: shells.map((t, i) => ({ ...t, name: shellNames[i] })),
      };
    }),
  }));
}

/** The factory run a workspace belongs to: its id, its issue where the id names one, and whether it waits on a person. */
export interface FactoryRun {
  run: string;
  issue?: number;
  url?: string;
  held?: HeldUp["kind"];
}

/**
 * What a factory session P1 marks stuck says instead: it is quiet, for at least P1's stuck
 * threshold. A fixed text, so it reads the same on every machine and after a reload.
 */
export const QUIET = "Quiet 10+ min";

/**
 * The factory run each workspace belongs to, as the believed statuses name it: a live run's
 * workspaces, and a held-up run's. A status names only workspaces of its own project.
 */
export function factoryRuns(statuses: Record<string, FactoryStatus> | undefined): (workspace: WorkspaceView) => FactoryRun | undefined {
  const named = new Map<string, FactoryRun>();
  for (const status of Object.values(statuses ?? {})) {
    const runs: { run: string; workspaces: string[]; held?: HeldUp["kind"] }[] = [
      ...status.runs.map((r) => ({ run: r.run, workspaces: r.workspaces })),
      ...status.held_up.map((h) => ({ run: h.run, workspaces: h.workspaces ?? [], held: h.kind })),
    ];
    for (const { run, workspaces, held } of runs) {
      const parts = parseRun(run);
      const mark: FactoryRun = { run, ...(parts && { issue: parts.issue, url: issueUrl(parts) }), ...(held && { held }) };
      for (const workspace of workspaces) named.set(`${status.project}/${workspace}`, mark);
    }
  }
  return (workspace) => named.get(`${workspace.projectId}/${workspace.id}`);
}

/** Whether the factory tends a workspace: its run is live, not held up for a person, so nothing in it needs you (G14). */
export const tendedByFactory = (run: FactoryRun | undefined): boolean => run !== undefined && !run.held;

/**
 * The fleet with each workspace a believed status names marked with its factory run, so every
 * view that lists its terminals (`rows`) reads them the same way. Without statuses, the fleet as is.
 */
export function withFactory(machines: MachineView[], statuses: Record<string, FactoryStatus> | undefined): MachineView[] {
  if (!statuses) return machines;
  const runOf = factoryRuns(statuses);
  return machines.map((machine) => ({
    ...machine,
    workspaces: machine.workspaces.map((workspace) => {
      const factory = runOf(workspace);
      return factory ? { ...workspace, factory } : workspace;
    }),
  }));
}

/** What a workspace shows as its branch: its branch, or the first 7 characters of its commit when it is on one. */
export function branchOf(workspace: WorkspaceView): string {
  return workspace.branch || workspace.base.slice(0, 7);
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

/** Each name as given, or with its number among its kind ("opus 1", "opus 2") when the list has more than one. */
function numbered(names: string[]): string[] {
  const seen = new Map<string, number>();
  return names.map((name) => {
    const n = (seen.get(name) ?? 0) + 1;
    seen.set(name, n);
    return names.indexOf(name) === names.lastIndexOf(name) ? name : `${name} ${n}`;
  });
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
  /** What Claude said when its last turn ended, only once the session rests: idle or finished. */
  said?: string;
  label: string;
  glyph: Glyph;
}

/** A session that has stopped for now or for good, so what it last said is its news. */
const rests: Glyph[] = ["ready", "done"];

/** A workspace's terminals in sidebar order: its sessions, then its shells. */
export function rows(workspace: WorkspaceView): Row[] {
  return [
    ...workspace.sessions.map((s) => {
      const glyph = sessionGlyph[s.state];
      return { terminal: s.terminal, session: s.id, name: s.name, title: s.title, now: busy.includes(glyph) ? s.now : undefined, said: rests.includes(glyph) ? s.said : undefined, label: sessionLabelIn(s.state, workspace.factory), glyph };
    }),
    ...workspace.shells.map((t) => ({ terminal: t.id, name: t.name, label: terminalLabel(t.state, t.exitCode), glyph: terminalGlyph(t.state, t.exitCode) })),
  ];
}

/**
 * A session's label in a workspace of factory run `run`. A live run's stuck session is quiet, since
 * the factory watches it; in a held-up run, which waits on a person, it stays stuck. Every view
 * that names a session's state reads it here. It keeps P1's stuck glyph on purpose: that is P1's
 * true state, and the label says why it is not urgent. A tended run's waiting session keeps its
 * label too: the factory answers it.
 */
export function sessionLabelIn(state: SessionState, run: FactoryRun | undefined): string {
  return tendedByFactory(run) && sessionGlyph[state] === "stuck" ? QUIET : sessionLabel[state];
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
