import { Link } from "@tanstack/react-router";
import { jumpKey } from "./JumpPalette";
import { buttonShape } from "./Modal";
import type { Fit } from "./TerminalHeader";
import type { Needing } from "./needing";
import { StateGlyph } from "./StateGlyph";

/** The key that goes to the next terminal that needs you, from anywhere. */
export const nextKey = jumpKey === "⌘K" ? "⌘J" : "Ctrl J";

/**
 * The way to the next terminal that needs you, in the bar above the open one: its state, its
 * workspace and how many need you, with what it asks on hover. It stays quiet while the open
 * terminal still needs you, since that one comes first; once it no longer does (you answered it),
 * it fills and glows once, so the next one is the obvious move. As the bar narrows (`fit`) it
 * gives up the workspace, then the word Next; its name says both at every width.
 */
export function NextUp({ next, others, answered, fit = "wide" }: { next: Needing; others: number; answered: boolean; fit?: Fit }) {
  const many = `${others} more ${others === 1 ? "needs" : "need"} you`;
  return (
    <Link
      // A new element when it fills, so its glow plays once then.
      key={String(answered)}
      to="/terminals/$terminalId"
      params={{ terminalId: next.terminal }}
      aria-label={`Next that needs you: ${next.workspace} / ${next.name}, ${many}`}
      title={`${next.now?.text ?? next.label} · ${next.workspace} / ${next.name} (${nextKey})`}
      data-answered={answered || undefined}
      className={`${buttonShape} flex max-w-48 min-w-0 shrink-0 items-center gap-1.5 font-medium transition-colors ${
        answered ? "border-amber-300/70 bg-amber-400/15 text-amber-100 hover:bg-amber-400/25 motion-safe:animate-nudge" : "border-amber-400/30 text-amber-300 hover:bg-amber-400/10"
      }`}
    >
      <StateGlyph glyph={next.glyph} />
      {fit !== "tight" && <span>Next</span>}
      {fit === "wide" && <span className="min-w-0 truncate font-normal">{next.workspace}</span>}
      <span aria-hidden="true" className="min-w-4 rounded-full bg-amber-400 px-1 text-center text-[10px] leading-4 font-semibold text-neutral-950 tabular-nums">
        {others}
      </span>
    </Link>
  );
}
