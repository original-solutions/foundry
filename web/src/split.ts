/** Two terminals shown side by side. The one with focus is the one the URL names. */
export interface Split {
  left: string;
  right: string;
}

/** `other` shown beside `focused`: on the right of a new split, else in place of the side without focus. */
export function beside(split: Split | undefined, focused: string, other: string): Split {
  if (split?.right === focused) return { left: other, right: focused };
  return { left: focused, right: other };
}

/**
 * The split once the URL moved from terminal `from` to `to`. Moving between its sides keeps it.
 * A terminal opened from anywhere else takes the side that had focus, as in an editor with two
 * groups. The fleet view, or a move from neither side, ends it.
 */
export function follow(split: Split | undefined, from: string | undefined, to: string | undefined): Split | undefined {
  if (!split || !to) return undefined;
  if (to === split.left || to === split.right) return split;
  if (from === split.left) return { left: to, right: split.right };
  if (from === split.right) return { left: split.left, right: to };
  return undefined;
}
