/** Something in a terminal's text that can be opened: a web address, or a file its workspace changed. */
export interface Found {
  /** Where it sits in the text: `start` inclusive, `end` exclusive. */
  start: number;
  end: number;
  kind: "url" | "file";
  /** The address, or the changed file's path as the workspace's changes name it. */
  target: string;
}

const URL = /https?:\/\/[^\s<>"'`]+/g;
const WORD = /[\w.\-/@~+]+/g;
const CLOSERS: Record<string, string> = { ")": "(", "]": "[", "}": "{" };

/** Drops what a sentence puts after an address: a full stop, a comma, or a bracket the address did not open. */
function trimUrl(url: string): string {
  for (;;) {
    const last = url.at(-1)!;
    const opener = CLOSERS[last];
    if (".,;:!?'\"".includes(last) || (opener && url.split(opener).length <= url.split(last).length - 1)) url = url.slice(0, -1);
    else return url;
  }
}

/** The changed file a word names: its path, its path from `./` or inside a longer path, or its name alone when only one changed file has it. */
function fileOf(word: string, files: readonly string[]): string | undefined {
  const path = word.replace(/^\.\//, "");
  if (files.includes(path)) return path;
  if (path.includes("/")) return files.filter((f) => path.endsWith(`/${f}`)).sort((a, b) => b.length - a.length)[0];
  const named = files.filter((f) => f.slice(f.lastIndexOf("/") + 1) === path);
  return named.length === 1 ? named[0] : undefined;
}

/**
 * The web addresses in `text`, and the words that name one of `files` (a workspace's changed paths),
 * in the order they appear. A path inside an address is part of the address.
 */
export function linksIn(text: string, files: readonly string[]): Found[] {
  const found: Found[] = [];
  for (const m of text.matchAll(URL)) {
    const url = trimUrl(m[0]);
    found.push({ start: m.index, end: m.index + url.length, kind: "url", target: url });
  }
  if (files.length === 0) return found;
  const inUrl = (at: number) => found.some((f) => f.kind === "url" && at >= f.start && at < f.end);
  for (const m of text.matchAll(WORD)) {
    if (inUrl(m.index)) continue;
    const word = m[0].replace(/\.+$/, "");
    const target = word && fileOf(word, files);
    if (target) found.push({ start: m.index, end: m.index + word.length, kind: "file", target });
  }
  return found.sort((a, b) => a.start - b.start);
}

/** A buffer row as xterm keeps it: only what reading its text needs. */
export interface Row {
  isWrapped: boolean;
  length: number;
  getCell(x: number): { getChars(): string; getWidth(): number } | undefined;
}

/**
 * The whole line that row `y` is part of (a long line wraps over several rows), and the cell each
 * of its characters sits in, 0-based. A wide character takes two cells and an empty cell reads as a space.
 */
export function rowText(row: (y: number) => Row | undefined, y: number): { text: string; cells: { x: number; y: number }[] } {
  let first = y;
  while (first > 0 && row(first)?.isWrapped) first--;
  let text = "";
  const cells: { x: number; y: number }[] = [];
  for (let at = first; at === first || row(at)?.isWrapped; at++) {
    const line = row(at);
    if (!line) break;
    for (let x = 0; x < line.length; x++) {
      const cell = line.getCell(x);
      if (!cell || cell.getWidth() === 0) continue;
      const chars = cell.getChars() || " ";
      text += chars;
      for (let i = 0; i < chars.length; i++) cells.push({ x, y: at });
    }
  }
  return { text, cells };
}
