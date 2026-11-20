import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { SessionState } from "./api";
import { Announcer, NotifyButton, useAlerts } from "./AlertsBell";
import { fleet } from "./fleet";
import { ME, sample, session, settings, terminal, workspace } from "./test-fixtures";

/** Stands in for the browser's Notification: records each one shown. */
class FakeNotification {
  static permission: NotificationPermission = "default";
  static shown: FakeNotification[] = [];
  static requestPermission = vi.fn(async () => FakeNotification.permission);
  onclick: (() => void) | null = null;
  close = vi.fn();
  constructor(
    readonly title: string,
    readonly options: NotificationOptions,
  ) {
    FakeNotification.shown.push(this);
  }
}

const world = (state: SessionState) =>
  fleet([sample(ME, 1), workspace(ME, "w1", "tidy"), terminal(ME, "t1", "w1"), session(ME, "s1", "w1", "t1", state)], settings());

let visible: DocumentVisibilityState = "visible";

function Harness({ state, onOpen }: { state: SessionState; onOpen: (terminal: string) => void }) {
  const { spoken, news, ...notify } = useAlerts(world(state), true, onOpen);
  return (
    <>
      <NotifyButton {...notify} />
      <Announcer spoken={spoken} />
      <output data-testid="cards">{news ? `${news.n}:${news.alerts.map((a) => a.body).join(",")}` : ""}</output>
    </>
  );
}

let focused: boolean;
beforeEach(() => {
  FakeNotification.permission = "default";
  FakeNotification.shown = [];
  FakeNotification.requestPermission.mockClear();
  vi.stubGlobal("Notification", FakeNotification);
  focused = true;
  visible = "visible";
  vi.spyOn(document, "hasFocus").mockImplementation(() => focused);
  Object.defineProperty(document, "visibilityState", { configurable: true, get: () => visible });
  localStorage.clear();
  document.head.querySelector("link[rel=icon]")?.remove();
  document.title = "Foundry";
});
afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  localStorage.clear();
});

const openPanel = () => fireEvent.click(screen.getByRole("button", { name: "Notifications" }));
const toggle = (name: RegExp) => screen.getByRole("switch", { name });
const cards = () => screen.getByTestId("cards").textContent;

describe("NotifyButton", () => {
  it("sets every way news reaches you in one place: cards, desktop alerts and the catch-up", () => {
    render(<Harness state="working" onOpen={() => {}} />);
    openPanel();
    const panel = screen.getByRole("dialog", { name: "Notifications" });
    expect(within(panel).getAllByRole("switch").map((s) => [s.getAttribute("aria-label"), s.getAttribute("aria-checked")])).toEqual([
      ["Cards in the page", "true"],
      ["Desktop alerts", "false"],
      ["Catch-up when you come back", "true"],
    ]);
    expect(panel.textContent).toContain("screen reader");

    fireEvent.click(toggle(/^Cards/));
    fireEvent.click(toggle(/^Catch-up/));
    expect(JSON.parse(localStorage.getItem("foundry.notify")!)).toEqual({ cards: false, desktop: false, away: false });
  });

  it("turns desktop alerts on once the browser allows them", async () => {
    render(<Harness state="working" onOpen={() => {}} />);
    openPanel();
    FakeNotification.permission = "granted";
    await act(async () => fireEvent.click(toggle(/^Desktop/)));
    expect(FakeNotification.requestPermission).toHaveBeenCalled();
    expect(toggle(/^Desktop/).getAttribute("aria-checked")).toBe("true");
    expect(JSON.parse(localStorage.getItem("foundry.notify")!).desktop).toBe(true);
  });

  it("says when the browser blocks desktop alerts, or has none, and keeps the rest", () => {
    FakeNotification.permission = "denied";
    const { unmount } = render(<Harness state="working" onOpen={() => {}} />);
    openPanel();
    expect(toggle(/^Desktop/).hasAttribute("disabled")).toBe(true);
    expect(screen.getByRole("dialog").textContent).toContain("site settings");
    unmount();

    vi.stubGlobal("Notification", undefined);
    render(<Harness state="working" onOpen={() => {}} />);
    openPanel();
    expect(toggle(/^Desktop/).hasAttribute("disabled")).toBe(true);
    expect(toggle(/^Cards/).hasAttribute("disabled")).toBe(false);
  });
});

describe("useAlerts", () => {
  it("tells you of each event once: a card while you are at the page, no desktop alert", () => {
    FakeNotification.permission = "granted";
    localStorage.setItem("foundry.notify", JSON.stringify({ cards: true, desktop: true, away: true }));
    const { rerender } = render(<Harness state="working" onOpen={() => {}} />);
    rerender(<Harness state="idle" onOpen={() => {}} />);
    expect(FakeNotification.shown).toHaveLength(0);
    expect(cards()).toBe("1:opus finished its turn");
  });

  it("alerts on the desktop, and only there, while the page is not in front, and a click opens it", () => {
    FakeNotification.permission = "granted";
    localStorage.setItem("foundry.alerts", "on");
    const onOpen = vi.fn();
    const raise = vi.spyOn(window, "focus").mockImplementation(() => {});
    const { rerender } = render(<Harness state="working" onOpen={onOpen} />);
    focused = false;
    rerender(<Harness state="idle" onOpen={onOpen} />);
    expect(cards()).toBe("");
    expect(FakeNotification.shown).toHaveLength(1);
    const [shown] = FakeNotification.shown;
    expect(shown.title).toBe("tidy");
    expect(shown.options).toMatchObject({ body: "opus finished its turn", tag: "s1" });
    shown.onclick?.();
    expect(raise).toHaveBeenCalled();
    expect(onOpen).toHaveBeenCalledWith("t1");
    expect(shown.close).toHaveBeenCalled();
  });

  it("takes its desktop alerts back when you return to the page", () => {
    FakeNotification.permission = "granted";
    localStorage.setItem("foundry.notify", JSON.stringify({ cards: true, desktop: true, away: true }));
    const { rerender } = render(<Harness state="working" onOpen={() => {}} />);
    focused = false;
    visible = "hidden";
    rerender(<Harness state="waiting_for_you" onOpen={() => {}} />);
    const [shown] = FakeNotification.shown;
    expect(shown.close).not.toHaveBeenCalled();
    focused = true;
    visible = "visible";
    act(() => void window.dispatchEvent(new Event("focus")));
    expect(shown.close).toHaveBeenCalled();
  });

  it("shows a card when the page shows but desktop alerts are off, and none while it is hidden", () => {
    const { rerender } = render(<Harness state="working" onOpen={() => {}} />);
    focused = false;
    rerender(<Harness state="idle" onOpen={() => {}} />);
    expect(cards()).toBe("1:opus finished its turn");
    visible = "hidden";
    rerender(<Harness state="working" onOpen={() => {}} />);
    rerender(<Harness state="waiting_for_you" onOpen={() => {}} />);
    expect(cards()).toBe("1:opus finished its turn");
    expect(FakeNotification.shown).toHaveLength(0);
  });

  it("raises no card with cards turned off, while a screen reader still hears it", () => {
    localStorage.setItem("foundry.notify", JSON.stringify({ cards: false, desktop: false, away: true }));
    const { rerender } = render(<Harness state="working" onOpen={() => {}} />);
    rerender(<Harness state="idle" onOpen={() => {}} />);
    expect(cards()).toBe("");
    expect(document.querySelector("[data-announcer]")!.textContent).toBe("tidy: opus finished its turn");
  });

  it("puts the fleet's state in the tab's title and icon", () => {
    const { rerender } = render(<Harness state="working" onOpen={() => {}} />);
    expect(document.title).toBe("Foundry · 1 working");
    rerender(<Harness state="waiting_for_you" onOpen={() => {}} />);
    expect(document.title).toBe("(1) Foundry · 1 needs you");
    const icon = document.head.querySelector<HTMLLinkElement>("link[rel=icon]");
    expect(decodeURIComponent(icon?.href ?? "")).toContain("#fbbf24");
  });
});

describe("Announcer", () => {
  it("tells a screen reader when a session needs you or finishes, however notifications are set", () => {
    const { rerender } = render(<Harness state="working" onOpen={() => {}} />);
    const status = document.querySelector("[data-announcer]")!;
    expect(status.textContent).toBe("");

    rerender(<Harness state="waiting_for_you" onOpen={() => {}} />);
    expect(status.textContent).toBe("tidy: opus is waiting for you");

    rerender(<Harness state="working" onOpen={() => {}} />);
    rerender(<Harness state="idle" onOpen={() => {}} />);
    expect(status.textContent).toBe("tidy: opus finished its turn");
  });

  it("says the same thing again when it happens again", () => {
    const { rerender } = render(<Harness state="working" onOpen={() => {}} />);
    rerender(<Harness state="idle" onOpen={() => {}} />);
    const first = document.querySelector("[data-announcer]")!.textContent;
    rerender(<Harness state="working" onOpen={() => {}} />);
    rerender(<Harness state="idle" onOpen={() => {}} />);

    const second = document.querySelector("[data-announcer]")!.textContent;
    expect(second?.trim()).toBe(first?.trim());
    expect(second).not.toBe(first);
  });
});
