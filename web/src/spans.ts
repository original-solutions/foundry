import type { Mark } from "./api";
import { turns } from "./trail";

/** The spans of time the timeline can show, newest at the right edge, and the gap between its ticks. */
export const RANGES = [
  { key: "15m", ms: 15 * 60_000, every: 5 * 60_000 },
  { key: "1h", ms: 60 * 60_000, every: 15 * 60_000 },
  { key: "4h", ms: 4 * 60 * 60_000, every: 60 * 60_000 },
  { key: "12h", ms: 12 * 60 * 60_000, every: 3 * 60 * 60_000 },
] as const;

/** One turn of a session as a bar: from its prompt to its end, with the tools and questions in it. */
export interface Bar {
  start: number;
  end: number;
  /** Still going: the session is busy and this is its newest turn, so it runs to now. */
  open: boolean;
  /**
   * Not a turn: a reminder that came after a turn ended (Claude Code's "waiting for your input"),
   * with no prompt and nothing but questions. Shown as its dot alone, and not counted.
   */
  nudge: boolean;
  prompt?: string;
  steps: Mark[];
}

/**
 * A session's trail as bars, oldest first. The newest unfinished turn of a busy session runs to
 * `now`; any other unfinished turn (stopped mid-turn, or cut short by the next prompt) ends at its
 * last step. A reminder after a turn ended is a `nudge`.
 */
export function bars(trail: Mark[], busy: boolean, now: number): Bar[] {
  const all = turns(trail);
  return all.map((turn, i) => {
    const open = turn.end === undefined && busy && i === all.length - 1;
    const end = turn.end ?? (open ? now : (turn.steps.at(-1)?.at ?? turn.start));
    const nudge = !open && i > 0 && turn.prompt === undefined && turn.steps.length > 0 && turn.steps.every((s) => s.kind === "ask");
    return { start: turn.start, end, open, nudge, prompt: turn.prompt, steps: turn.steps };
  });
}

/**
 * What a bar is called: the prompt you sent, quoted. Claude Code also sends itself prompts wrapped
 * in a tag (a background task's result, a command's output); those are named, not shown raw. A
 * turn the trail caught after its prompt is "Earlier steps".
 */
export function heading(bar: Bar): string {
  if (bar.prompt === undefined) return "Earlier steps";
  if (bar.prompt.startsWith("<task-notification>")) return "A background task finished";
  if (/^<[a-z-]+>/.test(bar.prompt)) return "A message from Claude Code";
  return `“${bar.prompt}”`;
}

/** Where a span sits in the window `from`..`to`, as percentages of its width, cut to it; `undefined` when wholly outside. */
export function placed(span: { start: number; end: number }, from: number, to: number): { left: number; width: number } | undefined {
  if (span.end < from || span.start > to) return undefined;
  const share = (t: number) => ((Math.min(Math.max(t, from), to) - from) / (to - from)) * 100;
  const left = share(span.start);
  return { left, width: share(span.end) - left };
}

const short = (ms: number) => (ms >= 3_600_000 ? `${ms / 3_600_000}h` : `${ms / 60_000}m`);

/** The ticks across a window of `range` ending now, at the range's step back from now, oldest first. */
export function ticks(range: number): { left: number; label: string }[] {
  const every = RANGES.find((r) => r.ms === range)?.every ?? range / 4;
  const out: { left: number; label: string }[] = [];
  for (let back = range - every; back > 0; back -= every) out.push({ left: ((range - back) / range) * 100, label: short(back) });
  return out;
}

/** What every session's bars add up to inside `from`..`to`: turns, agent time, tool calls and questions. */
export function sum(tracks: Bar[][], from: number, to: number) {
  let turns = 0;
  let busy = 0;
  let tools = 0;
  let asks = 0;
  for (const bar of tracks.flat()) {
    if (bar.nudge || bar.end < from || bar.start > to) continue;
    turns += 1;
    busy += Math.min(bar.end, to) - Math.max(bar.start, from);
    for (const step of bar.steps) {
      if (step.at < from || step.at > to) continue;
      if (step.kind === "ask") asks += 1;
      else if (step.kind === "tool") tools += 1;
    }
  }
  return { turns, busy, tools, asks };
}
