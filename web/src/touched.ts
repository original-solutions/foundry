import type { Mark } from "./api";
import { kindOf, type StepKind } from "./trail";

/** One place along a session's trail where something was touched: 0 at its first mark, 1 at its last. */
export interface Touch {
  at: number;
  edit: boolean;
}

/** A file the session read or changed, with how often, and where along its trail. */
export interface FileTouch {
  path: string;
  edits: number;
  reads: number;
  touches: Touch[];
}

/** A command or another tool's step the session ran, counted once however often it ran. */
export interface Ran {
  text: string;
  kind: StepKind;
  count: number;
  touches: Touch[];
}

/** The tools that act on one file, named in the trail as "Tool path". */
const onFile = new Set<StepKind>(["edit", "read"]);

/**
 * The file a step edits, writes or reads, as the trail names it (from the session's folder when
 * inside it). Nothing for any other step, or when the daemon cut the line short, since a cut path
 * names no file.
 */
export function pathOf(mark: Mark): string | undefined {
  if (mark.kind !== "tool" || mark.tool === undefined || !onFile.has(kindOf(mark))) return undefined;
  const head = `${mark.tool} `;
  if (!mark.text.startsWith(head)) return undefined;
  const path = mark.text.slice(head.length);
  return path === "" || path.endsWith("…") ? undefined : path;
}

/**
 * A session's trail turned from when to what: every file it read or changed (changed ones first),
 * every command it ran and every other tool step, each once, with how often and where along the
 * trail. Order is by the trail's own order, the order its hooks arrived, never by their times:
 * within each group the one touched last comes first. `turns` places each prompt after the first
 * along the trail, where one turn meets the next.
 */
export function touched(trail: Mark[]): { files: FileTouch[]; commands: Ran[]; others: Ran[]; turns: number[] } {
  const files = new Map<string, FileTouch & { last: number }>();
  const commands = new Map<string, Ran & { last: number }>();
  const others = new Map<string, Ran & { last: number }>();
  const turns: number[] = [];
  const span = Math.max(trail.length - 1, 1);
  trail.forEach((mark, i) => {
    const at = i / span;
    if (mark.kind === "prompt" && i > 0) turns.push(at);
    if (mark.kind !== "tool") return;
    const kind = kindOf(mark);
    const path = pathOf(mark);
    if (path !== undefined) {
      const file = files.get(path) ?? { path, edits: 0, reads: 0, touches: [], last: i };
      const edit = kind === "edit";
      if (edit) file.edits += 1;
      else file.reads += 1;
      file.touches.push({ at, edit });
      file.last = i;
      files.set(path, file);
      return;
    }
    const text = kind === "command" ? mark.text.replace(/^\$ /, "") : mark.text;
    const group = kind === "command" ? commands : others;
    const ran = group.get(text) ?? { text, kind, count: 0, touches: [], last: i };
    ran.count += 1;
    ran.touches.push({ at, edit: false });
    ran.last = i;
    group.set(text, ran);
  });
  const newest = <T extends { last: number }>(a: T, b: T) => b.last - a.last;
  return {
    files: [...files.values()].sort((a, b) => Number(b.edits > 0) - Number(a.edits > 0) || newest(a, b)).map(({ path, edits, reads, touches }) => ({ path, edits, reads, touches })),
    commands: [...commands.values()].sort(newest).map(({ text, kind, count, touches }) => ({ text, kind, count, touches })),
    others: [...others.values()].sort(newest).map(({ text, kind, count, touches }) => ({ text, kind, count, touches })),
    turns,
  };
}
