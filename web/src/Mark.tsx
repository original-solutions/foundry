/** Foundry's mark: an F drawn on the state glyphs' 3 by 3 dot grid, lit dots full, the rest dim. */
export const markShape = "#####.#..";

/** The mark, in the current text colour. Still: it names the app, it never shows a state. */
export function Mark({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 13 13" aria-hidden="true" data-mark className={className}>
      {[...markShape].map((cell, i) => (
        <circle key={i} cx={2 + (i % 3) * 4.5} cy={2 + Math.floor(i / 3) * 4.5} r={1.6} fill="currentColor" opacity={cell === "#" ? 1 : 0.18} />
      ))}
    </svg>
  );
}
