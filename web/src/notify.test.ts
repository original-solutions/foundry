import { afterEach, describe, expect, it } from "vitest";
import { loadNotify, NOTIFY_KEY, route, saveNotify } from "./notify";

afterEach(() => localStorage.clear());

describe("loadNotify", () => {
  it("has cards and the catch-up on and desktop alerts off until you choose", () => {
    expect(loadNotify()).toEqual({ cards: true, desktop: false, away: true });
  });

  it("keeps the choice for this browser", () => {
    saveNotify({ cards: false, desktop: true, away: false });
    expect(loadNotify()).toEqual({ cards: false, desktop: true, away: false });
  });

  it("keeps desktop alerts on for a browser that turned them on with the old bell", () => {
    localStorage.setItem("foundry.alerts", "on");
    expect(loadNotify()).toEqual({ cards: true, desktop: true, away: true });
  });

  it("falls back to the defaults when what is kept cannot be read", () => {
    localStorage.setItem(NOTIFY_KEY, "{not json");
    expect(loadNotify()).toEqual({ cards: true, desktop: false, away: true });
  });
});

describe("route", () => {
  const all = { cards: true, desktop: true, away: true };

  it("puts news in a card while you are at the page, never on the desktop too", () => {
    expect(route({ visible: true, focused: true }, all, true)).toBe("card");
  });

  it("sends news to the desktop while the page is not in front, and only there", () => {
    expect(route({ visible: false, focused: false }, all, true)).toBe("desktop");
    expect(route({ visible: true, focused: false }, all, true)).toBe("desktop");
  });

  it("falls back to a card when the page shows but desktop alerts are off or not allowed", () => {
    expect(route({ visible: true, focused: false }, { ...all, desktop: false }, false)).toBe("card");
    expect(route({ visible: true, focused: false }, all, false)).toBe("card");
  });

  it("holds news for the catch-up while the page is hidden and nothing reaches the desktop", () => {
    expect(route({ visible: false, focused: false }, { ...all, desktop: false }, false)).toBeUndefined();
  });

  it("shows no card when cards are off", () => {
    expect(route({ visible: true, focused: true }, { ...all, cards: false }, true)).toBeUndefined();
  });
});
