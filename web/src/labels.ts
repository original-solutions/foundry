import type { SessionState, TerminalState } from "./api";

export const sessionLabel: Record<SessionState, string> = {
  starting: "Starting",
  working: "Working",
  waiting_for_you: "Waiting for you",
  idle: "Idle",
  finished: "Finished",
  failed: "Failed",
  stuck: "Stuck",
  unreachable: "Unreachable",
};

const terminalNames: Record<TerminalState, string> = {
  running: "Running",
  exited: "Exited",
  unreachable: "Unreachable",
};

export function terminalLabel(state: TerminalState, exitCode: number | null): string {
  return state === "exited" && exitCode !== null ? `${terminalNames[state]} ${exitCode}` : terminalNames[state];
}

/**
 * The shape a state shows in front of its row. One per meaning, so sessions and shells share
 * them: `working` runs a turn and is the only one that moves, `waiting` needs a person, `ready`
 * waits for the next thing, `done` ended cleanly, `failed` ended badly, `stuck` stopped making
 * progress, `starting` has not reported yet and `away` cannot be reached.
 */
export type Glyph = "starting" | "working" | "waiting" | "ready" | "done" | "failed" | "stuck" | "away";

export const sessionGlyph: Record<SessionState, Glyph> = {
  starting: "starting",
  working: "working",
  waiting_for_you: "waiting",
  idle: "ready",
  finished: "done",
  failed: "failed",
  stuck: "stuck",
  unreachable: "away",
};

export function terminalGlyph(state: TerminalState, exitCode: number | null): Glyph {
  if (state === "running") return "ready";
  if (state === "unreachable") return "away";
  return exitCode ? "failed" : "done";
}
