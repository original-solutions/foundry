import { Link } from "@tanstack/react-router";
import { useState } from "react";
import { jumpKey } from "./JumpPalette";
import type { Needing } from "./needing";
import { StateGlyph } from "./StateGlyph";

/**
 * How the open terminal `terminal` stands for the strip, `needs` saying whether it needs you now:
 * `"needing"` while it does, `"answered"` once it stopped needing you while open, and nothing when
 * it has not needed you since it was opened (or no terminal is open).
 */
export function useOpenStanding(terminal: string | undefined, needs: boolean): "needing" | "answered" | undefined {
  // The terminal that needed you while open, until another one is opened.
  const [asked, setAsked] = useState<string>();
  const now = needs ? terminal : asked === terminal ? asked : undefined;
  if (now !== asked) setAsked(now);
  if (!terminal) return undefined;
  if (needs) return "needing";
  return asked === terminal ? "answered" : undefined;
}

/** The key that goes to the next terminal that needs you, from anywhere. */
export const nextKey = jumpKey === "⌘K" ? "⌘J" : "Ctrl J";

/**
 * The one way into what needs you, at the head of every page while anything does: a strip that
 * says how many need you and leads to the next, by its workspace and what it asks. From a view
 * with no terminal open it leads to the first in the board's order; over a terminal (`open`), to
 * the one after it. It stays quiet while the open terminal still needs you, since that one comes
 * first; once it no longer does (you answered it), it fills and glows once, so the next one is the
 * obvious move. The whole strip is the link, so a thumb finds it on a phone.
 */
export function NextUp({ next, others, open }: { next: Needing; others: number; open?: "needing" | "answered" }) {
  const answered = open === "answered";
  const count = `${others} ${open === "needing" ? "more " : ""}${others === 1 ? "needs" : "need"} you`;
  // A factory run's whole reason: the strip has the room, and cuts it itself.
  const asks = next.now?.text ?? next.fullLabel ?? next.label;
  return (
    <Link
      // A new element when it fills, so its glow plays once then.
      key={String(answered)}
      to="/terminals/$terminalId"
      params={{ terminalId: next.terminal }}
      aria-label={`Next that needs you: ${next.workspace} / ${next.name}, ${count}`}
      title={`${asks} · ${next.workspace} / ${next.name} (${nextKey})`}
      data-answered={answered || undefined}
      className={`pointer-events-auto flex min-w-0 flex-[999_1_16rem] items-center gap-2 rounded-lg border px-3 py-2 text-sm shadow-lg shadow-black/50 transition-colors ${
        answered ? "border-amber-300/70 bg-amber-400/15 hover:bg-amber-400/25 motion-safe:animate-nudge" : "border-amber-400/40 bg-neutral-900 hover:bg-amber-400/10"
      }`}
    >
      <StateGlyph glyph={next.glyph} />
      <span className="shrink-0 font-medium text-amber-300">{count}</span>
      <span aria-hidden="true" className="text-neutral-600">
        ·
      </span>
      <span className="min-w-0 flex-1 truncate">
        <span className="font-medium text-neutral-100">{next.workspace}</span> <span className="text-neutral-400">{asks}</span>
      </span>
      <span className="flex shrink-0 items-center gap-1.5 font-medium text-amber-300">
        Next
        <kbd className="hidden font-sans text-[11px] text-amber-300/70 pointer-fine:inline">{nextKey}</kbd>
      </span>
    </Link>
  );
}
