import type { DiffLine } from "./changes";
import type { SessionView } from "./fleet";
import { sessionGlyph, sessionLabel, type Glyph } from "./labels";

/** Where a comment sits: a line of the file as it is now ("new") or a removed line of the base ("old"). */
export interface Anchor {
  side: "new" | "old";
  line: number;
  /** The line's text, so a comment lets go of a line the agent has since rewritten. */
  code: string;
}

/** A comment for Claude on one line of a workspace's diff, not sent yet. */
export interface Note extends Anchor {
  id: number;
  path: string;
  body: string;
}

/** Where a diff line can take a comment, or nothing for hunk headers and notes. */
export function anchor(line: DiffLine): Anchor | undefined {
  if (line.kind === "removed") return { side: "old", line: line.old, code: line.text };
  if (line.kind === "added" || line.kind === "same") return { side: "new", line: line.new, code: line.text };
  return undefined;
}

/** Whether `note` belongs under the line at `at` of file `path`. */
export function onLine(note: Note, path: string, at: Anchor): boolean {
  return note.path === path && note.side === at.side && note.line === at.line && note.code === at.code;
}

/** Longest line of code quoted back to Claude. */
const QUOTED = 120;

/** Control characters other than tab and newline: an escape could end the paste early and a CR would press Enter. */
const control = (c: string) => c !== "\n" && c !== "\t" && (c < " " || c === "\x7f");
const clean = (text: string) => [...text].filter((c) => !control(c)).join("");

/** The comments as one message for Claude: numbered, each with its file and line, the line itself, then what to do. */
export function review(notes: Note[]): string {
  const items = notes.map((n, i) => {
    const where = n.side === "new" ? `${n.path}:${n.line}` : `${n.path}, removed line ${n.line}`;
    const code = clean(n.code).trim();
    const quoted = code.length > QUOTED ? `${code.slice(0, QUOTED - 1)}…` : code;
    const body = clean(n.body)
      .trim()
      .split("\n")
      .map((l) => `   ${l}`);
    return [`${i + 1}. ${clean(where)}`, `   > ${quoted}`, ...body].join("\n");
  });
  return ["Review comments on your changes. Address each one, then say what you changed.", "", items.join("\n\n")].join("\n");
}

/** A session that could take the comments, named so two of one model tell apart. */
export interface Taker {
  terminal: string;
  name: string;
  /** Idle: waiting for its next prompt, so typing into it starts a turn. */
  ready: boolean;
  label: string;
  glyph: Glyph;
}

const TITLE_SHOWN = 40;

export function takers(sessions: SessionView[]): Taker[] {
  const count = new Map<string, number>();
  for (const s of sessions) count.set(s.model, (count.get(s.model) ?? 0) + 1);
  const seen = new Map<string, number>();
  return sessions.map((s) => {
    const n = (seen.get(s.model) ?? 0) + 1;
    seen.set(s.model, n);
    const title = s.title && (s.title.length > TITLE_SHOWN ? `${s.title.slice(0, TITLE_SHOWN - 1)}…` : s.title);
    const name = title ? `${s.model} · ${title}` : count.get(s.model)! > 1 ? `${s.model} ${n}` : s.model;
    return { terminal: s.terminal, name, ready: s.state === "idle", label: sessionLabel[s.state], glyph: sessionGlyph[s.state] };
  });
}
