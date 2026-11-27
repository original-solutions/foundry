import type { AnyRecord, Mark } from "./api";
import { needsYouIn, rows, type MachineView, type Row } from "./fleet";
import { urgency } from "./labels";
import { turns } from "./trail";
import { readStoredJson, storeJson, unstore } from "./storage";

/** How long the page must be hidden before coming back says what happened meanwhile. */
export const AWAY_MS = 2 * 60_000;
/** Where the page keeps when it was last hidden, so a tab closed and opened again catches up too. */
export const AWAY_KEY = "foundry.away";

/** When the page was hidden, and the version each session's record had then. */
export interface Left {
  at: number;
  versions: Record<string, number>;
}

/** The moment the page is hidden: the time, and every live session's record version. */
export function leaving(records: AnyRecord[], at: number): Left {
  const versions: Record<string, number> = {};
  for (const r of records) if (r.kind === "session" && !r.deleted) versions[r.body.id] = r.version;
  return { at, versions };
}

/** A session worth a line on coming back: one whose record moved since `left`, or one that needs you now. */
export interface Moved {
  row: Row & { session: string };
  workspace: string;
  /** Its record is newer than when the page was hidden (or it was not there yet). */
  changed: boolean;
  /** It started while the page was hidden. */
  fresh: boolean;
  /** It needs you now: its glyph says so and no live factory run tends it. */
  needs: boolean;
}

/**
 * Every session to tell about on coming back, what needs you first, then the rest by urgency
 * (what is busy, then what rests), ties in fleet order. Empty when no session's record moved: a session that was already
 * waiting before the page was hidden is not news on its own. A live factory run's session never
 * needs you, as on the board: its rows read as the board's do. Records are compared by version,
 * never by time.
 */
export function moved(left: Left, records: AnyRecord[], machines: MachineView[]): Moved[] {
  const version = new Map(records.flatMap((r) => (r.kind === "session" && !r.deleted ? [[r.body.id, r.version] as const] : [])));
  const all = machines.flatMap((m) =>
    m.workspaces.flatMap((w) => {
      return rows(w).flatMap((row) => {
        if (!row.session) return [];
        const before = left.versions[row.session];
        const changed = before === undefined || (version.get(row.session) ?? 0) > before;
        const needs = needsYouIn(row.glyph, w.factory);
        if (!changed && !needs) return [];
        return [{ row: { ...row, session: row.session }, workspace: w.name, changed, fresh: before === undefined, needs }];
      });
    }),
  );
  if (!all.some((m) => m.changed)) return [];
  return all.sort((a, b) => Number(b.needs) - Number(a.needs) || urgency.indexOf(a.row.glyph) - urgency.indexOf(b.row.glyph));
}

/**
 * What a trail did from `from`: the turns that ended and the tool calls and questions it made. The
 * reminder Claude Code sends after a turn ended ("waiting for your input") is not a question.
 */
export function since(trail: Mark[], from: number): { turns: number; steps: Mark[] } {
  let ended = 0;
  const steps: Mark[] = [];
  turns(trail).forEach((turn, i) => {
    if (i > 0 && turn.prompt === undefined && turn.steps.length > 0 && turn.steps.every((s) => s.kind === "ask")) return;
    if (turn.end !== undefined && turn.end >= from) ended += 1;
    steps.push(...turn.steps.filter((s) => s.at >= from));
  });
  return { turns: ended, steps };
}

/** The moment kept for this browser, taken out so it is used once; `undefined` when there is none or it cannot be read. */
export function takeLeft(): Left | undefined {
  const kept = readStoredJson(AWAY_KEY);
  unstore(AWAY_KEY);
  if (!kept || typeof kept !== "object") return undefined;
  const parsed = kept as Partial<Left>;
  return typeof parsed.at === "number" && parsed.versions && typeof parsed.versions === "object" ? { at: parsed.at, versions: parsed.versions } : undefined;
}

export function keepLeft(left: Left) {
  // When not kept, coming back to this page still says what happened; a reopened tab does not.
  storeJson(AWAY_KEY, left);
}
