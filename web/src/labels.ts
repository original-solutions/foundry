import type { SessionState, TerminalState } from "./api";

/**
 * A state in the words every screen uses. They match the board's lanes and the first-run page:
 * a session that waits for its next prompt is Ready, one that asks something Needs you, and a
 * session or shell that closed has Ended (or Failed).
 */
export const sessionLabel: Record<SessionState, string> = {
  starting: "Starting",
  working: "Working",
  waiting_for_you: "Needs you",
  idle: "Ready",
  finished: "Ended",
  failed: "Failed",
  stuck: "Stuck",
  unreachable: "Unreachable",
};

/** A shell's state in the same words as a session's: a clean exit has Ended, any other Failed with its code. */
export function terminalLabel(state: TerminalState, exitCode: number | null): string {
  if (state === "running") return "Running";
  if (state === "unreachable") return sessionLabel.unreachable;
  return exitCode ? `${sessionLabel.failed} · exit ${exitCode}` : sessionLabel.finished;
}

/** Phrases more than one screen says, kept here so they read the same everywhere. */
export const words = {
  noChanges: "No changes yet",
  unreadable: "Its changes can't be read from here",
  changes: "What this workspace changed since it left its base branch",
  archive: "Archive: see what stays and what stops, then stop its terminals and remove its worktree. The branch stays.",
};

/** What could not be done and why, as one sentence: "Could not start a shell: workspace not found." */
export function couldNot(what: string, error: unknown): string {
  const why = (error instanceof Error ? error.message : String(error)).trim().replace(/\.$/, "");
  return `Could not ${what}: ${why}.`;
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

/** Every glyph from most to least urgent: what needs a person, then what is busy, then the rest. */
export const urgency: Glyph[] = ["waiting", "stuck", "failed", "working", "starting", "ready", "done", "away"];

/** Glyphs that need a person. */
export const needsYou: Glyph[] = ["waiting", "stuck", "failed"];

/** A session shown as busy: in the middle of a turn, so its live turn and what it is doing now show. */
export const busy: Glyph[] = ["starting", "working", "waiting", "stuck"];

/**
 * A session that adds steps without changing state, so its steps and brief are read on a timer.
 * Unlike the rest of `busy`, a waiting or stuck one changes only when a hook arrives, which is a state change.
 */
export const addsSteps: Glyph[] = ["starting", "working"];

/** `n` things in plain words: "1 file", "3 files". */
export const plural = (n: number, one: string) => `${n} ${one}${n === 1 ? "" : "s"}`;
