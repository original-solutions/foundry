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

/** A stretch of a turn that stood waiting on you: from a question to the step after it. */
export interface Wait {
  start: number;
  end: number;
  /** Still waiting: the session waits on you now, so it runs to now. */
  open: boolean;
}

/**
 * When a turn stood waiting on you. Each question runs to the step after it (the answer let the
 * turn go on), or to the turn's end. The newest open turn of a session that waits on you (as its
 * hooks say) waits up to now, from its unanswered question, or from its last step when the trail
 * holds no question. A question with no step since, in a session no longer waiting, was answered
 * at a time not known yet, so it claims nothing until the next step comes. A reminder between
 * turns is no wait.
 */
export function waits(bar: Bar, waiting: boolean, now: number): Wait[] {
  if (bar.nudge) return [];
  const out: Wait[] = [];
  bar.steps.forEach((step, i) => {
    if (step.kind !== "ask") return;
    const after = bar.steps[i + 1];
    if (after) out.push({ start: step.at, end: after.at, open: false });
    else if (!bar.open) out.push({ start: step.at, end: bar.end, open: false });
    else if (waiting) out.push({ start: step.at, end: now, open: true });
  });
  if (bar.open && waiting && bar.steps.at(-1)?.kind !== "ask") out.push({ start: bar.steps.at(-1)?.at ?? bar.start, end: now, open: true });
  return out.filter((w) => w.end > w.start);
}

/**
 * Every session's waits, `tracks`, laid over one another inside `from`..`to`: spans, oldest first,
 * each with how many sessions stood waiting on you through it. Nothing where none did.
 */
export function layers(tracks: Wait[][], from: number, to: number): { start: number; end: number; depth: number }[] {
  const edges: [number, number][] = [];
  for (const w of tracks.flat()) {
    const start = Math.max(w.start, from);
    const end = Math.min(w.end, to);
    if (end > start) edges.push([start, 1], [end, -1]);
  }
  edges.sort((a, b) => a[0] - b[0] || a[1] - b[1]);
  const out: { start: number; end: number; depth: number }[] = [];
  let depth = 0;
  let at = from;
  for (const [t, step] of edges) {
    if (t > at && depth > 0) {
      const last = out.at(-1);
      if (last && last.end === at && last.depth === depth) last.end = t;
      else out.push({ start: at, end: t, depth });
    }
    at = t;
    depth += step;
  }
  return out;
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

/**
 * What every session's bars add up to inside `from`..`to`: turns, agent time, time waiting on you,
 * tool calls and questions. The time a turn stood waiting on you (`held`, each track's waits) is
 * counted as waiting, not as agent work.
 */
export function sum(tracks: Bar[][], from: number, to: number, held: Wait[][] = []) {
  let turns = 0;
  let busy = 0;
  let waited = 0;
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
  for (const w of held.flat()) waited += Math.max(0, Math.min(w.end, to) - Math.max(w.start, from));
  return { turns, busy: busy - waited, waited, tools, asks };
}
