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
export class ApiError extends Error {}

async function post(path: string, body?: unknown): Promise<{ status: number; json: Record<string, unknown> }> {
  const response = await fetch(path, {
    method: "POST",
    headers: body === undefined ? undefined : { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  let json: Record<string, unknown> = {};
  try {
    const parsed: unknown = JSON.parse(await response.text());
    if (parsed && typeof parsed === "object") json = parsed as Record<string, unknown>;
  } catch {
    // An empty or non-JSON body, as a 204 has.
  }
  return { status: response.status, json };
}

function refused(status: number, json: Record<string, unknown>): ApiError {
  return new ApiError(typeof json.error === "string" ? json.error : `request failed (${status})`);
}

/** Makes a workspace. `error` beside the id means it exists but its setup did not start. */
export async function createWorkspace(input: { project: string; base: string; name: string }): Promise<{ id: string; error?: string }> {
  const { status, json } = await post("/api/workspaces", input);
  if (status !== 201) throw refused(status, json);
  return { id: String(json.id), error: typeof json.error === "string" ? json.error : undefined };
}

export async function startSession(workspace: string, model: string): Promise<string> {
  const { status, json } = await post(`/api/workspaces/${workspace}/sessions`, { model });
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
