import type { Glyph } from "./labels";
import type { Row } from "./fleet";

/**
 * Each glyph's shape on a 3 by 3 dot grid, row by row: `#` lit, `.` dim. Every shape differs,
 * so a state reads by its outline before its colour. Working moves; its shape here is the still
 * frame shown when the user asks for reduced motion.
 */
export const shapes: Record<Glyph, string> = {
  starting: "....#....", // a seed: launched, not reported yet
  working: ".#.###.#.", // the ripple's middle frame
  waiting: "####.####", // an empty frame: your move
  ready: "#########", // full: ready for the next thing
  done: "..##.#.#.", // a tick
  failed: "#.#.#.#.#", // a cross
  stuck: "#.##.##.#", // pause bars: no progress
  away: "#.#...#.#", // only the corners: out of reach
};

/**
 * The glyph's colour, also used for its words where they share a line. Colour means "look here":
 * only a state that needs you (waiting, stuck, failed) or is moving (working) has one. A resting
 * state is grey, so a fleet at rest reads calm and the one that works or waits stands out. Green
 * is left to what you can do (actions) and what was added (diffs).
 */
export const glyphText: Record<Glyph, string> = {
  starting: "text-neutral-300",
  working: "text-sky-400",
  waiting: "text-amber-400",
  ready: "text-neutral-400",
  done: "text-neutral-400",
  failed: "text-red-400",
  stuck: "text-orange-400",
  away: "text-neutral-500",
};

/** The same colours as a fill, for a card's edge and a machine's bar of states. */
export const glyphFill: Record<Glyph, string> = {
  starting: "bg-neutral-400",
  working: "bg-sky-400",
  waiting: "bg-amber-400",
  ready: "bg-neutral-500",
  done: "bg-neutral-600",
  failed: "bg-red-400",
  stuck: "bg-orange-400",
  away: "bg-neutral-700",
};

/** How far each cell sits from the centre, in rings: the ripple's delay. */
const ring = [2, 1, 2, 1, 0, 1, 2, 1, 2];

/**
 * A state's glyph: nine dots in a 3 by 3 grid. Working ripples out from the centre and is the
 * only one that moves; every other state is a still shape. Keyed by glyph, so it plays one pop
 * when the state changes and never otherwise. `still` holds working to its still frame, for a glyph
 * that names a state rather than shows one (a heading, a legend).
 */
export function StateGlyph({ glyph, still = false }: { glyph: Glyph; still?: boolean }) {
  const shape = shapes[glyph];
  const moving = glyph === "working" && !still;
  return (
    <svg
      key={glyph}
      viewBox="0 0 13 13"
      aria-hidden="true"
      data-glyph={glyph}
      className={`size-3.25 shrink-0 motion-safe:animate-state-in ${glyphText[glyph]} ${glyph === "waiting" ? "drop-shadow-[0_0_3px_var(--color-amber-400)]" : ""}`}
    >
      {ring.map((r, i) => {
        const lit = shape[i] === "#";
        return (
          <circle
            key={i}
            cx={2 + (i % 3) * 4.5}
            cy={2 + Math.floor(i / 3) * 4.5}
            r={1.6}
            fill="currentColor"
            data-lit={lit || undefined}
            opacity={lit ? 1 : 0.18}
            className={moving ? "motion-safe:animate-ripple" : undefined}
            style={moving ? { animationDelay: `${r * 160}ms` } : undefined}
          />
        );
      })}
    </svg>
  );
}

/**
 * A terminal's state in words: its `label`, with the uncut words on hover when the label cuts
 * them (a factory run's reason), and no tooltip that only says the label again.
 */
export function StateLabel({ of, className }: { of: Pick<Row, "label" | "fullLabel">; className?: string }) {
  return (
    <span title={of.fullLabel === of.label ? undefined : of.fullLabel} className={className}>
      {of.label}
    </span>
  );
}
