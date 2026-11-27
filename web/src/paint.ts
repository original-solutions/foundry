import type { DiffLine } from "./changes";
import { languageOf, lex, type Tone } from "./syntax";

/** A run of a diff line's text: its colour as code, and whether it is a word the change changed. */
export interface Piece {
  text: string;
  tone?: Tone;
  changed?: boolean;
}

/** Where in a line a change is, from start to end. */
export type Range = [number, number];

/** Lines longer than this are not compared word by word: the table it takes grows with both lengths. */
const COMPARED_UP_TO = 400;

/** How many added lines ahead a removed line looks for its pair, so a large rewrite stays quick to show. */
const LOOK_AHEAD = 6;

/** A removed line and the line that took its place pair only when at least this share of their text is the same. */
const ALIKE = 0.4;

/**
 * The words that differ between a removed line and the added line that took its place, as ranges
 * in each: the parts of each line not in their longest common run of words. None when the lines
 * have too little in common to read as one line changed, rather than one line gone and another new.
 */
export function changedWords(was: string, now: string): [Range[], Range[]] | undefined {
  if (was.length > COMPARED_UP_TO || now.length > COMPARED_UP_TO) return undefined;
  const a = was.match(/\w+|\s+|[^\w\s]/g) ?? [];
  const b = now.match(/\w+|\s+|[^\w\s]/g) ?? [];
  // The longest common subsequence of the two lines' words, from the end.
  const table = Array.from({ length: a.length + 1 }, () => new Array<number>(b.length + 1).fill(0));
  for (let i = a.length - 1; i >= 0; i--) {
    for (let j = b.length - 1; j >= 0; j--) {
      table[i][j] = a[i] === b[j] ? table[i + 1][j + 1] + 1 : Math.max(table[i + 1][j], table[i][j + 1]);
    }
  }
  const keptA = new Array<boolean>(a.length).fill(false);
  const keptB = new Array<boolean>(b.length).fill(false);
  let same = 0;
  for (let i = 0, j = 0; i < a.length && j < b.length; ) {
    if (a[i] === b[j]) {
      keptA[i] = keptB[j] = true;
      same += a[i].trim().length;
      i++;
      j++;
    } else if (table[i + 1][j] >= table[i][j + 1]) i++;
    else j++;
  }
  const solid = Math.max(was.replace(/\s/g, "").length, now.replace(/\s/g, "").length);
  if (solid === 0 || same / solid < ALIKE) return undefined;
  return [ranges(a, keptA), ranges(b, keptB)];
}

/** The runs of words not kept, as ranges; a space between two changed words joins them. */
function ranges(words: string[], kept: boolean[]): Range[] {
  const text = words.join("");
  const out: Range[] = [];
  let at = 0;
  words.forEach((w, i) => {
    const end = at + w.length;
    if (!kept[i] && w.trim()) {
      const last = out[out.length - 1];
      if (last && !text.slice(last[1], at).trim()) last[1] = end;
      else out.push([at, end]);
    }
    at = end;
  });
  return out;
}

/**
 * Each diff line as pieces to show, in the same order as the lines (hunk headers and notes have
 * none). Code in a language this knows is coloured, the old side and the new side each read in
 * order so a comment open on one line colours the next; each hunk starts afresh, since what came
 * before it is not in the diff. In a run of removed lines followed by added ones, each removed line
 * pairs with the next added line alike enough, in order, and the words that changed are marked in both.
 */
export function paint(lines: DiffLine[], path: string): Piece[][] {
  const lang = languageOf(path);
  const out: Piece[][] = lines.map((l) => (l.kind === "hunk" || l.kind === "note" ? [] : [{ text: l.text }]));
  if (lang) {
    const colour = (side: number[], keep: (i: number) => boolean) => {
      const tokens = lex(side.map((i) => lines[i].text), lang);
      side.forEach((i, k) => {
        if (keep(i)) out[i] = tokens[k];
      });
    };
    let old: number[] = [];
    let now: number[] = [];
    const flush = () => {
      colour(old, (i) => lines[i].kind === "removed");
      colour(now, () => true);
      old = [];
      now = [];
    };
    lines.forEach((l, i) => {
      if (l.kind === "hunk") flush();
      if (l.kind === "same" || l.kind === "removed") old.push(i);
      if (l.kind === "same" || l.kind === "added") now.push(i);
    });
    flush();
  }
  for (let i = 0; i < lines.length; i++) {
    if (lines[i].kind !== "removed") continue;
    const removed: number[] = [];
    for (; lines[i]?.kind === "removed"; i++) removed.push(i);
    const added: number[] = [];
    for (; lines[i]?.kind === "added"; i++) added.push(i);
    i--;
    // Each removed line pairs with the next added line alike enough, so a line put in above it does not shift the pairs.
    let from = 0;
    for (const r of removed) {
      for (let k = from; k < Math.min(added.length, from + LOOK_AHEAD); k++) {
        const marks = changedWords(lines[r].text, lines[added[k]].text);
        if (!marks) continue;
        out[r] = mark(out[r], marks[0]);
        out[added[k]] = mark(out[added[k]], marks[1]);
        from = k + 1;
        break;
      }
    }
  }
  return out;
}

/** Pieces cut where the ranges start and end, with the pieces inside them marked changed. */
function mark(pieces: Piece[], marks: Range[]): Piece[] {
  if (marks.length === 0) return pieces;
  const cuts = [...new Set(marks.flat())];
  const out: Piece[] = [];
  let at = 0;
  for (const p of pieces) {
    const end = at + p.text.length;
    let from = at;
    for (const c of [...cuts.filter((c) => c > at && c < end), end]) {
      const changed = marks.some(([s, e]) => from >= s && from < e);
      out.push({ ...p, text: p.text.slice(from - at, c - at), ...(changed ? { changed } : {}) });
      from = c;
    }
    at = end;
  }
  return out;
}
