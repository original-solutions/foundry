import type { Glyph } from "./labels";

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

/** The glyph's colour, also used for its words where they share a line. */
export const glyphText: Record<Glyph, string> = {
  starting: "text-neutral-300",
  working: "text-sky-400",
  waiting: "text-amber-400",
  ready: "text-emerald-400",
  done: "text-neutral-400",
  failed: "text-red-400",
  stuck: "text-orange-400",
  away: "text-neutral-500",
};

const pillBg: Record<Glyph, string> = {
  starting: "bg-neutral-700/60",
  working: "bg-sky-400/15",
  waiting: "bg-amber-400/15",
  ready: "bg-emerald-400/10",
  done: "bg-neutral-700/60",
  failed: "bg-red-500/15",
  stuck: "bg-orange-400/15",
  away: "bg-neutral-800",
};

/** How far each cell sits from the centre, in rings: the ripple's delay. */
const ring = [2, 1, 2, 1, 0, 1, 2, 1, 2];

/**
 * A state's glyph: nine dots in a 3 by 3 grid. Working ripples out from the centre and is the
 * only one that moves; every other state is a still shape. Keyed by glyph, so it plays one pop
 * when the state changes and never otherwise.
 */
export function StateGlyph({ glyph }: { glyph: Glyph }) {
  const shape = shapes[glyph];
  const moving = glyph === "working";
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

/** The state as a pill: its glyph and its label. */
export function StatePill({ glyph, label }: { glyph: Glyph; label: string }) {
  return (
    <span className={`inline-flex shrink-0 items-center gap-1.5 rounded-full px-2.5 py-0.5 text-xs font-medium ${pillBg[glyph]} ${glyphText[glyph]}`}>
      <StateGlyph glyph={glyph} />
      <span>{label}</span>
    </span>
  );
}
