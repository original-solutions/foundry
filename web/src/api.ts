// Wire types, matching the serde JSON of foundry-proto, and the calls the UI makes.

export type SessionState = "starting" | "working" | "waiting_for_you" | "idle" | "finished" | "failed" | "stuck" | "unreachable";
export type TerminalState = "running" | "exited" | "unreachable";

export interface Rec<T> {
  owner: string;
  version: number;
  deleted: boolean;
  body: T;
}

export interface Workspace {
  id: string;
  project: string;
  name: string;
  branch: string;
  base: string;
}
export interface Terminal {
  id: string;
  workspace: string;
  state: TerminalState;
  exit_code: number | null;
}
export interface Session {
  id: string;
  workspace: string;
  terminal: string;
  model: string;
  state: SessionState;
}
export interface MachineSample {
  load_per_core: number;
  free_memory: number;
  free_disk: number;
  sessions: number;
}

export type AnyRecord =
  | ({ kind: "workspace" } & Rec<Workspace>)
  | ({ kind: "terminal" } & Rec<Terminal>)
  | ({ kind: "session" } & Rec<Session>)
  | ({ kind: "machine_sample" } & Rec<MachineSample>);

export interface SharedSettings {
  seq: number;
  machines: { id: string; name: string; address: string }[];
  priority: string[];
  projects: Project[];
  limits: Record<string, unknown>;
}
export interface Project {
  id: string;
  name: string;
  repo: string;
}

export type UiMessage =
  | { type: "state"; protocol: number; records: AnyRecord[]; settings: SharedSettings }
  | { type: "record"; protocol: number; record: AnyRecord }
  | { type: "settings"; protocol: number; settings: SharedSettings };

/** What holds an archive up. */
export interface Blockers {
  uncommitted: string[];
  unpushed: string[];
  submodules: string[];
}

/** A request the daemon refused, with the text it gave. */
export class ApiError extends Error {
  /** The HTTP status the daemon answered with, when it answered. */
  readonly status?: number;
  constructor(message: string, status?: number) {
    super(message);
    this.status = status;
  }
}

export async function post(path: string, body?: unknown): Promise<{ status: number; json: Record<string, unknown> }> {
  const response = await fetch(path, {
    method: "POST",
    headers: body === undefined ? undefined : { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  return answered(response);
}

/** A response's status and JSON body; `{}` for a body that is empty or not JSON. */
async function answered(response: Response): Promise<{ status: number; json: Record<string, unknown> }> {
  let json: Record<string, unknown> = {};
  try {
    const parsed: unknown = JSON.parse(await response.text());
    if (parsed && typeof parsed === "object") json = parsed as Record<string, unknown>;
  } catch {
    // An empty or non-JSON body, as a 204 has.
  }
  return { status: response.status, json };
}

export function refused(status: number, json: Record<string, unknown>): ApiError {
  return new ApiError(typeof json.error === "string" ? json.error : `request failed (${status})`, status);
}

/** Reads `path`: its JSON body, or the daemon's refusal as an `ApiError` with its status. */
export async function get(path: string): Promise<Record<string, unknown>> {
  const response = await fetch(path);
  let json: Record<string, unknown> = {};
  try {
    json = (await response.json()) as Record<string, unknown>;
  } catch {
    // Not JSON: the status says what went wrong.
  }
  if (!response.ok) throw refused(response.status, json);
  return json;
}

/** A machine in the machine list, and whether this machine's daemon hears from it. */
export interface MachineChoice {
  id: string;
  name: string;
  online: boolean;
}

/** The machine list, with which one is this machine. */
export async function listMachines(): Promise<{ me: string; machines: MachineChoice[] }> {
  const json = await get("/api/machines");
  return { me: String(json.me), machines: Array.isArray(json.machines) ? (json.machines as MachineChoice[]) : [] };
}

/** A workspace made: `error` means it exists but its setup did not start, `warning` that its machine is over its limits. */
export interface Made {
  id: string;
  error?: string;
  warning?: string;
}

/** A create no machine takes yet: the request that waits for one, and each machine's reason. */
export interface Waiting {
  request: string;
  reasons: string[];
}

const text = (value: unknown) => (typeof value === "string" ? value : undefined);

/**
 * Makes a workspace, on `machine` when one is given and on this machine otherwise. With `machine`
 * "choose" Foundry picks the machine, and `model` and `prompt` go with it for the session a request
 * that waits starts once it is placed.
 */
export async function createWorkspace(input: {
  project: string;
  base: string;
  name: string;
  machine?: string;
  model?: string;
  prompt?: string;
}): Promise<Made | Waiting> {
  const { status, json } = await post("/api/workspaces", input);
  if (status === 202) return { request: String(json.request), reasons: Array.isArray(json.reasons) ? json.reasons.map(String) : [] };
  if (status !== 201) throw refused(status, json);
  return { id: String(json.id), error: text(json.error), warning: text(json.warning) };
}

/** A create waiting on this machine for one to qualify, as `GET /api/placements` lists it. */
export interface Placement {
  id: string;
  project: string;
  name: string;
  /** "placing" while its workspace is being made; "failed" when that did not work, with `error`. */
  state: "waiting" | "placing" | "failed";
  /** When it started waiting, in ms since the epoch. */
  waiting_since: number;
  /** Why no machine takes it, one per machine. */
  reasons: string[];
  error: string | null;
}

export const placementsKey = ["placements"];

export async function listPlacements(): Promise<Placement[]> {
  const json = await get("/api/placements");
  return Array.isArray(json.requests) ? (json.requests as Placement[]) : [];
}

/** Cancels a waiting request, so it is never placed, or dismisses one that failed. */
export async function cancelPlacement(id: string): Promise<void> {
  const { status, json } = await post(`/api/placements/${id}/cancel`);
  if (status !== 204) throw refused(status, json);
}

/** Starts Claude Code in `workspace`; with a `prompt`, Claude Code starts on it as its first prompt. */
export async function startSession(workspace: string, model: string, prompt?: string): Promise<string> {
  const { status, json } = await post(`/api/workspaces/${workspace}/sessions`, prompt === undefined ? { model } : { model, prompt });
  if (status !== 201) throw refused(status, json);
  return String(json.id);
}

export async function startShell(workspace: string): Promise<string> {
  const { status, json } = await post(`/api/workspaces/${workspace}/shells`);
  if (status !== 201) throw refused(status, json);
  return String(json.id);
}

/** Archives a workspace: `null` when done, the blockers when it is held up and `force` is off. */
export async function archiveWorkspace(workspace: string, force: boolean): Promise<Blockers | null> {
  const { status, json } = await post(`/api/workspaces/${workspace}/archive`, { force });
  if (status === 204) return null;
  if (status === 409) {
    const list = (value: unknown) => (Array.isArray(value) ? value.map(String) : []);
    return { uncommitted: list(json.uncommitted), unpushed: list(json.unpushed), submodules: list(json.submodules) };
  }
  throw refused(status, json);
}

/** One path a workspace changed since its base. Counts are `null` for a binary file. */
export interface FileChange {
  path: string;
  /** The path it was renamed from. */
  from: string | null;
  status: "added" | "modified" | "deleted" | "renamed" | "untracked";
  added: number | null;
  removed: number | null;
  /** Some of the change is not committed yet. */
  uncommitted: boolean;
  /** The hunks, or `null` when binary, a symlink or too large to send. */
  patch: string | null;
}

/** A commit on a workspace's branch: its short hash and the first line of its message. */
export interface Commit {
  hash: string;
  subject: string;
}

/** What a workspace changed since it left its base: commits on its branch, newest first, and every changed file. */
export interface Changes {
  base: string;
  commits: Commit[];
  files: FileChange[];
}

export async function workspaceChanges(workspace: string): Promise<Changes> {
  return (await get(`/api/workspaces/${workspace}/changes`)) as unknown as Changes;
}

/** What one commit on a workspace's branch changed against its parent: that commit, and its files. */
export async function workspaceCommit(workspace: string, hash: string): Promise<Changes> {
  return (await get(`/api/workspaces/${workspace}/commits/${encodeURIComponent(hash)}`)) as unknown as Changes;
}

/** A pull request from a workspace's branch, as GitHub has it. */
export interface PullRequest {
  number: number;
  title: string;
  url: string;
  state: "open" | "merged" | "closed";
  draft: boolean;
  /** Its checks rolled up: any failing, else any running, else passing; `none` when it has none. */
  checks: "none" | "pending" | "passing" | "failing";
  review: "none" | "required" | "approved" | "changes_requested";
}

/** Where a workspace's branch stands past this machine. */
export interface Shipping {
  branch: string;
  /** `origin` has the branch, as of this machine's last fetch or push. */
  pushed: boolean;
  /** Commits on the worktree's HEAD that no remote branch has. */
  unpushed: number;
  /** The newest pull request from the branch, once pushed, when GitHub answered and has one. */
  pr: PullRequest | null;
  /** GitHub was asked and could not answer, so `pr` says nothing. */
  unreachable: boolean;
}

export async function workspaceShipping(workspace: string): Promise<Shipping> {
  return (await get(`/api/workspaces/${workspace}/pr`)) as unknown as Shipping;
}

/** The newest step of a session's turn: a prompt, a tool about to run, or a question for you. */
export interface Step {
  kind: "prompt" | "tool" | "ask";
  /** Claude Code's name for the tool ("Bash", "Edit", an MCP tool's name…); only on a `tool` step. */
  tool?: string;
  /** The line to show, such as `$ cargo test` or `Edit src/a.rs`. For display only. */
  text: string;
}
/** What a session is for (its first prompt) and doing, from its hooks. */
export interface Brief {
  title: string | null;
  now: Step | null;
  /** What Claude said when its last turn ended, its lines kept and cut short, until the next prompt. */
  said?: string;
}
/** Briefs by session id: only this machine's sessions, since this daemon started. */
export type Briefs = Record<string, Brief>;

/** One step on a session's trail: a prompt, a tool call, a question, or the end of a turn (`done`, no text). `at` is when its hook arrived, in ms since the epoch. */
export interface Mark {
  kind: Step["kind"] | "done";
  /** Claude Code's name for the tool; only on a `tool` step. */
  tool?: string;
  text: string;
  at: number;
}

/** Every step session `id` took since its daemon started, oldest first. Only this machine's sessions; any other is a 404. */
export async function sessionTrail(id: string): Promise<Mark[]> {
  return ((await get(`/api/sessions/${id}/trail`)).steps ?? []) as Mark[];
}

/** The id of the machine whose daemon serves the UI: the only one that can end its terminals. */
export async function thisMachine(): Promise<string> {
  return String((await get("/api/machine")).id);
}

/** The read of which machine serves the UI; it does not change while the page is open. */
export const machineQuery = { queryKey: ["machine"], queryFn: thisMachine, staleTime: Infinity };

export async function sessionBriefs(): Promise<Briefs> {
  return (await get("/api/briefs")) as Briefs;
}

/** Ends a terminal: hangs up its child if it still runs and removes it, with its final screen. */
export async function closeTerminal(id: string): Promise<void> {
  const { status, json } = await post(`/api/terminals/${id}/close`);
  if (status !== 204) throw refused(status, json);
}

/** The largest file a terminal is sent; the daemon refuses a larger one. */
export const MAX_FILE = 25 * 1024 * 1024;

/** Sends `file` to terminal `id` of this machine, which keeps it beside the terminal until it is closed, and gives back the path it was kept at. */
export async function sendFile(id: string, file: File): Promise<string> {
  const response = await fetch(`/api/terminals/${id}/files?name=${encodeURIComponent(file.name)}`, { method: "POST", body: file });
  const { status, json } = await answered(response);
  if (status !== 201) throw refused(status, json);
  return String(json.path);
}
