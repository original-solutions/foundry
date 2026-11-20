/** One row of a terminal's buffer: its text, untrimmed, and whether it continues the row above. */
export type BufferRow = { text: string; wrapped: boolean };

/** Where a hit starts in the buffer, and how many characters it spans (it may run onto the next row). */
export type Hit = { row: number; col: number; length: number };

/** The most hits one search keeps: enough for any real search, few enough to stay quick. */
export const MAX_HITS = 2000;

/**
 * Every place `query` appears in `rows`, oldest first. A long line the terminal wrapped is
 * searched as one line, so a hit across the wrap is found, at the row it starts on. The query is
 * literal, and ignores case unless it has a capital (as in most editors). Stops at [MAX_HITS].
 */
export function hitsIn(rows: BufferRow[], query: string): Hit[] {
  if (!query) return [];
  const pattern = new RegExp(query.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"), query === query.toLowerCase() ? "gi" : "g");
  const hits: Hit[] = [];
  for (let first = 0; first < rows.length && hits.length < MAX_HITS; ) {
    // One logical line: this row and the rows that continue it. Each row's start in the line maps a hit back to its row.
    const starts = [0];
    let line = rows[first].text;
    let last = first;
    while (last + 1 < rows.length && rows[last + 1].wrapped) {
      last++;
      starts.push(line.length);
      line += rows[last].text;
    }
    line = line.trimEnd();
    for (const match of line.matchAll(pattern)) {
      let part = starts.length - 1;
      while (starts[part] > match.index) part--;
      hits.push({ row: first + part, col: match.index - starts[part], length: match[0].length });
      if (hits.length === MAX_HITS) break;
    }
    first = last + 1;
  }
  return hits;
}

/** The hit `dir` (−1 older, +1 newer) from `index` of `count`, wrapping around. */
export function step(index: number, count: number, dir: 1 | -1): number {
  return (index + dir + count) % count;
}

/** The index of `hit` in `hits`, or of the hit nearest to where it was. */
export function closest(hits: Hit[], hit: Hit): number {
  let best = 0;
  for (let i = 0; i < hits.length; i++) {
    if (hits[i].row === hit.row && hits[i].col === hit.col) return i;
    if (Math.abs(hits[i].row - hit.row) < Math.abs(hits[best].row - hit.row)) best = i;
  }
  return best;
}

/**
 * Where the hits sit along the whole scrollback of `total` rows, as percentages from the top,
 * one mark per spot (a tenth of a percent) leading to its first hit, with the spot of hit `current` lit.
 */
export function marks(hits: Hit[], total: number, current: number): { at: number; hit: number; current: boolean }[] {
  const spots = new Map<number, { at: number; hit: number; current: boolean }>();
  hits.forEach((hit, i) => {
    const at = Math.round((hit.row / Math.max(total, 1)) * 1000) / 10;
    const spot = spots.get(at);
    if (spot) spot.current ||= i === current;
    else spots.set(at, { at, hit: i, current: i === current });
  });
  return [...spots.values()];
}
