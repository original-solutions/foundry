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
