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
 * How loudly a state shows: `attention` needs a person, `bad` failed, `busy` is running a turn,
 * `ready` waits for the next thing, `quiet` has nothing to say, `away` cannot be reached.
 */
export type Tone = "attention" | "bad" | "busy" | "ready" | "quiet" | "away";

export const sessionTone: Record<SessionState, Tone> = {
  starting: "quiet",
  working: "busy",
  waiting_for_you: "attention",
  idle: "ready",
  finished: "quiet",
  failed: "bad",
  stuck: "bad",
  unreachable: "away",
};

export function terminalTone(state: TerminalState, exitCode: number | null): Tone {
  if (state === "running") return "ready";
  if (state === "unreachable") return "away";
  return exitCode ? "bad" : "quiet";
}
