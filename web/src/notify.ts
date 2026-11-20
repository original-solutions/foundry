/**
 * How news of a session (it finished its turn, or it needs you) reaches you, chosen in one place:
 * a card in the page while you are at it, a desktop alert while you are not, and a catch-up card
 * when you come back after a while. The tab's title and icon and a screen reader always have it.
 */
export interface Notify {
  /** A card in the page for a session you are not looking at. */
  cards: boolean;
  /** A desktop alert while the page is not in front (the browser must allow it too). */
  desktop: boolean;
  /** Coming back after a while away, a card says what each session did meanwhile. */
  away: boolean;
}

export const NOTIFY_KEY = "foundry.notify";
/** Where the old bell kept whether desktop alerts were on. */
const OLD_KEY = "foundry.alerts";

const defaults: Notify = { cards: true, desktop: false, away: true };

export function loadNotify(): Notify {
  try {
    const kept = localStorage.getItem(NOTIFY_KEY);
    if (kept) {
      const parsed = JSON.parse(kept) as Partial<Notify>;
      return {
        cards: typeof parsed.cards === "boolean" ? parsed.cards : defaults.cards,
        desktop: typeof parsed.desktop === "boolean" ? parsed.desktop : defaults.desktop,
        away: typeof parsed.away === "boolean" ? parsed.away : defaults.away,
      };
    }
    return { ...defaults, desktop: localStorage.getItem(OLD_KEY) === "on" };
  } catch {
    return defaults;
  }
}

export function saveNotify(notify: Notify) {
  try {
    localStorage.setItem(NOTIFY_KEY, JSON.stringify(notify));
    localStorage.removeItem(OLD_KEY);
  } catch {
    // Storage blocked: the choice lasts for this page only.
  }
}

/**
 * Where one piece of news goes, so it interrupts you once: the desktop while the page is not in
 * front and desktop alerts are on and allowed, else a card while the page shows. Nothing while the
 * page is hidden otherwise: the catch-up says it on coming back.
 */
export function route(where: { visible: boolean; focused: boolean }, notify: Notify, allowed: boolean): "card" | "desktop" | undefined {
  if (notify.desktop && allowed && !where.focused) return "desktop";
  if (where.visible && notify.cards) return "card";
  return undefined;
}
