import type { FileChange } from "./api";
import { diffLines, type DiffLine } from "./changes";

/**
 * What a file looked like when you marked it viewed: a fingerprint of its whole change, and one of
 * each line it added or removed, so a later change can show which lines are new since you looked.
 * Only these short hashes are kept, never the code.
 */
export interface Seen {
  h: string;
  l: string[];
}

/** What you viewed in one workspace, by path. */
export type Viewed = Record<string, Seen>;

export type Standing = "unviewed" | "viewed" | "changed";

/** Past this many changed lines a file keeps its fingerprint only, and a later change marks no lines. */
const LINES_KEPT = 2000;

/** Workspaces whose viewed files this browser keeps; the ones viewed longest ago go first. */
const WORKSPACES_KEPT = 40;

const KEY = "foundry.viewed";

/** FNV-1a, 32 bits, in base 36: short and stable, and enough to tell two versions of a file apart. */
function hash(text: string): string {
  let h = 0x811c9dc5;
  for (let i = 0; i < text.length; i++) {
    h ^= text.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return (h >>> 0).toString(36);
}

const lineHash = (line: DiffLine) => hash(`${line.kind}${line.text}`);
const changed = (line: DiffLine) => line.kind === "added" || line.kind === "removed";

function fingerprint(file: FileChange): string {
  return hash([file.status, file.from ?? "", file.patch ?? `${file.added}/${file.removed}`].join("\0"));
}

/** The file as it is now, to keep as viewed. */
export function seen(file: FileChange): Seen {
  const lines = file.patch === null ? [] : diffLines(file.patch).filter(changed);
  return { h: fingerprint(file), l: lines.length > LINES_KEPT ? [] : lines.map(lineHash) };
}

export function standing(file: FileChange, kept: Seen | undefined): Standing {
  if (!kept) return "unviewed";
  return kept.h === fingerprint(file) ? "viewed" : "changed";
}

/** The indexes of added or removed lines that were not in the file when it was viewed. */
export function freshLines(lines: DiffLine[], kept: Seen | undefined): Set<number> {
  const fresh = new Set<number>();
  if (!kept || kept.l.length === 0) return fresh;
  const before = new Set(kept.l);
  lines.forEach((line, i) => {
    if (changed(line) && !before.has(lineHash(line))) fresh.add(i);
  });
  return fresh;
}

/** How far through the change you are: files viewed as they are now, and files changed since you viewed them. */
export function progress(files: FileChange[], viewed: Viewed) {
  let done = 0;
  let since = 0;
  for (const f of files) {
    const s = standing(f, viewed[f.path]);
    if (s === "viewed") done++;
    else if (s === "changed") since++;
  }
  return { total: files.length, viewed: done, changed: since };
}

/** The first file after `after` (wrapping round) that is not viewed as it is now. */
export function nextUnviewed(files: FileChange[], viewed: Viewed, after: number): number | undefined {
  for (let step = 1; step <= files.length; step++) {
    const i = (after + step) % files.length;
    if (standing(files[i], viewed[files[i].path]) !== "viewed") return i;
  }
  return undefined;
}

function isSeen(value: unknown): value is Seen {
  if (typeof value !== "object" || value === null) return false;
  const v = value as Record<string, unknown>;
  return typeof v.h === "string" && Array.isArray(v.l) && v.l.every((l) => typeof l === "string");
}

function loadAll(): Record<string, Record<string, unknown>> {
  try {
    const kept: unknown = JSON.parse(localStorage.getItem(KEY) ?? "{}");
    return typeof kept === "object" && kept !== null && !Array.isArray(kept) ? (kept as Record<string, Record<string, unknown>>) : {};
  } catch {
    return {};
  }
}

/** What this browser viewed in a workspace, or nothing when storage is empty, blocked or holds something else. */
export function loadViewed(workspace: string): Viewed {
  const entry = loadAll()[workspace];
  if (typeof entry !== "object" || entry === null) return {};
  return Object.fromEntries(Object.entries(entry).filter(([, s]) => isSeen(s))) as Viewed;
}

/** Keeps what was viewed in a workspace, for the files it still changes, as the most recently viewed workspace. */
export function saveViewed(workspace: string, viewed: Viewed, files: FileChange[]) {
  const all = loadAll();
  delete all[workspace];
  const paths = new Set(files.map((f) => f.path));
  const kept = Object.fromEntries(Object.entries(viewed).filter(([path]) => paths.has(path)));
  if (Object.keys(kept).length > 0) all[workspace] = kept;
  const names = Object.keys(all);
  for (const old of names.slice(0, Math.max(0, names.length - WORKSPACES_KEPT))) delete all[old];
  try {
    localStorage.setItem(KEY, JSON.stringify(all));
  } catch {
    // Storage blocked or full: what you viewed lasts while the sheet is open.
  }
}
