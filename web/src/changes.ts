import type { Changes } from "./api";

/** The whole change in numbers: files, lines added and removed, and files with uncommitted work. */
export function totals(changes: Changes) {
  let added = 0;
  let removed = 0;
  for (const f of changes.files) {
    added += f.added ?? 0;
    removed += f.removed ?? 0;
  }
  return { files: changes.files.length, added, removed, uncommitted: changes.files.filter((f) => f.uncommitted).length };
}

export type DiffLine =
  | { kind: "hunk"; text: string }
  | { kind: "note"; text: string }
  | { kind: "same"; text: string; old: number; new: number }
  | { kind: "removed"; text: string; old: number }
  | { kind: "added"; text: string; new: number };

/**
 * The start of `lines` that holds its first `count` lines other than notes, carried on to the end
 * of the change it stops inside, so each removed line it holds still meets the added lines it pairs
 * with. What a peek paints, rather than the whole patch.
 */
export function leading(lines: DiffLine[], count: number): DiffLine[] {
  let end = 0;
  for (let shown = 0; end < lines.length && shown < count; end++) if (lines[end].kind !== "note") shown++;
  const changed = (line?: DiffLine) => line?.kind === "removed" || line?.kind === "added";
  if (changed(lines[end - 1])) while (end < lines.length && (changed(lines[end]) || lines[end].kind === "note")) end++;
  return lines.slice(0, end);
}

/** A file's hunks as lines to show, each numbered on the side or sides it is on. */
export function diffLines(patch: string): DiffLine[] {
  const lines: DiffLine[] = [];
  let old = 0;
  let next = 0;
  for (const line of patch.split("\n")) {
    const hunk = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(line);
    if (hunk) {
      old = Number(hunk[1]);
      next = Number(hunk[2]);
      lines.push({ kind: "hunk", text: line });
    } else if (line.startsWith("+")) lines.push({ kind: "added", text: line.slice(1), new: next++ });
    else if (line.startsWith("-")) lines.push({ kind: "removed", text: line.slice(1), old: old++ });
    else if (line.startsWith(" ")) lines.push({ kind: "same", text: line.slice(1), old: old++, new: next++ });
    else if (line.startsWith("\\")) lines.push({ kind: "note", text: line.slice(1).trim() });
  }
  return lines;
}

/** A path's folder, with its trailing slash, and its file name. */
export function splitPath(path: string) {
  const at = path.lastIndexOf("/") + 1;
  return { dir: path.slice(0, at), name: path.slice(at) };
}
