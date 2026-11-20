import type { Mark } from "./api";

/** One turn of a session: the prompt that started it, the steps it took, and when it began and ended. */
export interface Turn {
  prompt?: string;
  steps: Mark[];
  start: number;
  /** When it finished; `undefined` while it is still going (or the session stopped mid-turn). */
  end?: number;
}

/** A trail cut into turns at each prompt, oldest first. Steps before the first prompt kept make a turn with no prompt. */
export function turns(trail: Mark[]): Turn[] {
  const all: Turn[] = [];
  let open: Turn | undefined;
  for (const mark of trail) {
    if (mark.kind === "prompt" || !open || open.end !== undefined) {
      open = { prompt: undefined, steps: [], start: mark.at, end: undefined };
      all.push(open);
    }
    if (mark.kind === "prompt" && open.steps.length === 0 && open.prompt === undefined) open.prompt = mark.text;
    else if (mark.kind === "done") open.end = mark.at;
    else open.steps.push(mark);
  }
  return all;
}

export type StepKind = "command" | "edit" | "read" | "search" | "web" | "agent" | "ask" | "other";

/** What each of Claude Code's tools does. Any other tool, such as an MCP one, is "other". */
const toolKinds = new Map<string, StepKind>([
  ["Bash", "command"],
  ["Edit", "edit"],
  ["Write", "edit"],
  ["MultiEdit", "edit"],
  ["NotebookEdit", "edit"],
  ["Read", "read"],
  ["Grep", "search"],
  ["Glob", "search"],
  ["WebFetch", "web"],
  ["WebSearch", "web"],
  ["Task", "agent"],
  ["Agent", "agent"],
]);

/** What a step did, from the tool its hook named. */
export function kindOf(mark: Mark): StepKind {
  if (mark.kind === "ask") return "ask";
  return (mark.tool !== undefined && toolKinds.get(mark.tool)) || "other";
}

const words: Record<StepKind, [string, string]> = {
  command: ["command", "commands"],
  edit: ["edit", "edits"],
  read: ["read", "reads"],
  search: ["search", "searches"],
  web: ["web look-up", "web look-ups"],
  agent: ["agent", "agents"],
  ask: ["question", "questions"],
  other: ["other tool", "other tools"],
};

/** A turn's steps counted by what they did, biggest count first: "2 commands · 1 edit". */
export function tally(steps: Mark[]): string {
  if (steps.length === 0) return "no tools";
  const counts = new Map<StepKind, number>();
  for (const step of steps) counts.set(kindOf(step), (counts.get(kindOf(step)) ?? 0) + 1);
  return [...counts]
    .sort((a, b) => b[1] - a[1])
    .map(([kind, n]) => `${n} ${words[kind][n === 1 ? 0 : 1]}`)
    .join(" · ");
}

/** How long ago `at` was at `now`, short: "now", "42s", "5m", "3h", "2d". */
export function ago(at: number, now: number): string {
  const s = Math.floor((now - at) / 1000);
  if (s < 1) return "now";
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86_400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86_400)}d`;
}

/** A span of `ms`, to the two biggest units: "41s", "3m 12s", "1h 2m". */
export function took(ms: number): string {
  const s = Math.floor(ms / 1000);
  if (s < 1) return "<1s";
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
}
