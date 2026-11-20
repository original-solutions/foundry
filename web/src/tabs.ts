/**
 * How many terminals keep their pane attached. Each attached pane holds a socket and an xterm with up
 * to 10,000 lines of scrollback, so with 50+ terminals they cannot all stay live. Six covers a
 * split (two) plus a working set to flick between; one beyond it attaches again when shown,
 * and the holder sends its whole screen and scrollback as on a first open.
 */
export const ATTACHED_MAX = 6;

/**
 * The attached panes, most recent first, once terminal `id` is shown: cut to `max`, but never
 * dropping one in `pinned` (the sides of a split, which are on screen).
 */
export function touched(attached: string[], id: string, pinned: string[] = [], max = ATTACHED_MAX): string[] {
  const order = [id, ...attached.filter((t) => t !== id)];
  let room = max - order.filter((t) => pinned.includes(t)).length;
  return order.filter((t) => pinned.includes(t) || room-- > 0);
}

/** `tabs` without `id`, and the tab to show in its place: its right neighbour, else its left. */
export function withoutTab(tabs: string[], id: string): { tabs: string[]; next: string | undefined } {
  const at = tabs.indexOf(id);
  const rest = tabs.filter((t) => t !== id);
  return { tabs: rest, next: rest[Math.min(at, rest.length - 1)] };
}
