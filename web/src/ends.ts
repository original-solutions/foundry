import { rows, type WorkspaceView } from "./fleet";
import type { Glyph } from "./labels";

/**
 * What ending a terminal does from its state: `end` hangs up a child that still runs, so it is
 * asked first; `clear` removes one that already ended and only keeps its final screen. A
 * terminal whose machine cannot be reached cannot be ended from here.
 */
export function endOf(glyph: Glyph): "end" | "clear" | undefined {
  if (glyph === "away") return undefined;
  return glyph === "done" || glyph === "failed" ? "clear" : "end";
}

/** The terminals of `workspace` that have ended, in sidebar order. */
export function ended(workspace: WorkspaceView): string[] {
  return rows(workspace).flatMap((row) => (endOf(row.glyph) === "clear" ? [row.terminal] : []));
}
