import type { SessionState } from "./api";
import { byUrgency, needingRows, needsYouIn, rows, tendedByFactory, type MachineView } from "./fleet";
import { needsYou, sessionGlyph, type Glyph } from "./labels";
import { shapes } from "./StateGlyph";

/** A desktop alert: a session finished its turn or needs you. */
export interface Alert {
  /** The terminal a click opens. */
  terminal: string;
  /** The session's id, so a newer alert for it replaces the older one. */
  tag: string;
  glyph: Glyph;
  /** The workspace's name. */
  title: string;
  body: string;
}

/** Each session's state, by id, as last seen, and whether a live factory run tended it then. */
export type Seen = Map<string, { state: SessionState; tended: boolean }>;

/** What entering a state says, for the states worth an alert. */
const says: Partial<Record<SessionState, string>> = {
  waiting_for_you: "is waiting for you",
  stuck: "is stuck: no output for 10 minutes",
  failed: "failed",
};

/**
 * The alerts for what changed since `before`: a session that went from working to idle finished
 * its turn, and one that entered waiting, stuck or failed needs you (also when it shows up that
 * way). A live factory run's session never needs you, as on the board: the factory tends it. Once
 * its run is held up, a session that already waits, is stuck or failed needs you, and is raised
 * then. The first look, with nothing before it, raises nothing.
 */
export function alertsFor(before: Seen | undefined, machines: MachineView[]): { alerts: Alert[]; seen: Seen } {
  const seen: Seen = new Map();
  const alerts: Alert[] = [];
  for (const workspace of machines.flatMap((m) => m.workspaces)) {
    const tended = tendedByFactory(workspace.factory);
    for (const s of workspace.sessions) {
      const was = before?.get(s.id);
      // A resend of the fleet drops every factory mark until the statuses follow it, so a tended
      // session that lost its mark with no change of its own is no news: it stays tended until a
      // status says otherwise (held up, then it is raised).
      const unmarked = was?.tended === true && was.state === s.state && workspace.factory === undefined;
      seen.set(s.id, { state: s.state, tended: tended || unmarked });
      if (!before || unmarked) continue;
      if (was?.state === s.state && was.tended === tended) continue;
      const phrase = was?.state === "working" && s.state === "idle" ? "finished its turn" : needsYouIn(sessionGlyph[s.state], workspace.factory) ? says[s.state] : undefined;
      if (phrase) alerts.push({ terminal: s.terminal, tag: s.id, glyph: sessionGlyph[s.state], title: workspace.name, body: `${s.name} ${phrase}` });
    }
  }
  return { alerts, seen };
}

/** The fleet at a glance: its most urgent glyph, how many terminals need you and how many work. */
export interface Pulse {
  glyph: Glyph | undefined;
  needYou: number;
  working: number;
}

/** The fleet's pulse. A live factory run's terminals that would need you are left out, as the board leaves its card under working. */
export function pulse(machines: MachineView[]): Pulse {
  const workspaces = machines.flatMap((m) => m.workspaces);
  // What would need you but a live factory run tends is left out of the glyph; what needs you is
  // counted as the board's needs-you lane holds it (`needingRows`), a held-up run's card included.
  const all = workspaces.flatMap((w) => rows(w).filter((r) => needsYouIn(r.glyph, w.factory) || !needsYou.includes(r.glyph)));
  const count = (...glyphs: Glyph[]) => all.filter((r) => glyphs.includes(r.glyph)).length;
  return { glyph: byUrgency(all)[0]?.glyph, needYou: workspaces.reduce((n, w) => n + needingRows(w).length, 0), working: count("working") };
}

/** The tab's title: what needs you first, so a narrow tab still shows the count. */
export function tabTitle({ needYou, working }: Pulse): string {
  if (needYou > 0) return `(${needYou}) Foundry · ${needYou} need${needYou === 1 ? "s" : ""} you`;
  if (working > 0) return `Foundry · ${working} working`;
  return "Foundry";
}

/** Each glyph's colour as a hex value, matching `glyphText`, for drawing outside the page. */
const hex: Record<Glyph, string> = {
  starting: "#d4d4d4",
  working: "#38bdf8",
  waiting: "#fbbf24",
  ready: "#a3a3a3",
  done: "#a3a3a3",
  failed: "#f87171",
  stuck: "#fb923c",
  away: "#737373",
};

/** The working glyph's ripple for the tab icon: the centre, the ring around it, the corners. */
export const rippleFrames = ["....#....", ".#.#.#.#.", "#.#...#.#"];

/**
 * The tab icon as an SVG data URL: the glyph's dots on a dark tile, so it reads on a light or a
 * dark tab bar. `frame` replaces the still shape (a ripple frame); with no glyph, a grey grid.
 */
export function favicon(glyph: Glyph | undefined, frame?: string): string {
  const shape = frame ?? (glyph ? shapes[glyph] : shapes.ready);
  const colour = glyph ? hex[glyph] : hex.away;
  const dots = [...shape]
    .map((cell, i) => `<circle cx="${7 + (i % 3) * 9}" cy="${7 + Math.floor(i / 3) * 9}" r="3.4" fill="${colour}" opacity="${cell === "#" ? 1 : 0.22}"/>`)
    .join("");
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><rect width="32" height="32" rx="7" fill="#171717"/>${dots}</svg>`;
  return `data:image/svg+xml,${encodeURIComponent(svg)}`;
}
