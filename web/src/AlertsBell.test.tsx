import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { SessionState } from "./api";
import { AlertsButton, Announcer, useAlerts } from "./AlertsBell";
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

function Harness({ state, onOpen }: { state: SessionState; onOpen: (terminal: string) => void }) {
  const { spoken, ...alerts } = useAlerts(world(state), true, onOpen);
  return (
    <>
      <AlertsButton {...alerts} />
      <Announcer spoken={spoken} />
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
  vi.spyOn(document, "hasFocus").mockImplementation(() => focused);
  localStorage.clear();
  document.head.querySelector("link[rel=icon]")?.remove();
  document.title = "Foundry";
});
afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  localStorage.clear();
});

describe("useAlerts", () => {
  it("offers to turn alerts on, and turns them on once the browser allows it", async () => {
    render(<Harness state="working" onOpen={() => {}} />);
    const bell = screen.getByRole("button", { name: "Alerts off" });
    FakeNotification.permission = "granted";
    await act(async () => fireEvent.click(bell));
    expect(FakeNotification.requestPermission).toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "Alerts on" }).getAttribute("aria-pressed")).toBe("true");
    expect(localStorage.getItem("foundry.alerts")).toBe("on");
  });

  it("says when the browser blocks alerts", () => {
    FakeNotification.permission = "denied";
    render(<Harness state="working" onOpen={() => {}} />);
    expect(screen.getByRole("button", { name: "Alerts blocked" }).getAttribute("title")).toContain("site settings");
  });

  it("shows nothing where the browser has no notifications", () => {
    vi.stubGlobal("Notification", undefined);
    render(<Harness state="working" onOpen={() => {}} />);
    expect(screen.queryByRole("button")).toBeNull();
  });

  it("alerts when a session finishes its turn while the page is not looked at, and a click opens it", () => {
    FakeNotification.permission = "granted";
    localStorage.setItem("foundry.alerts", "on");
    const onOpen = vi.fn();
    const raise = vi.spyOn(window, "focus").mockImplementation(() => {});
    const { rerender } = render(<Harness state="working" onOpen={onOpen} />);
    focused = false;
    rerender(<Harness state="idle" onOpen={onOpen} />);
    expect(FakeNotification.shown).toHaveLength(1);
    const [shown] = FakeNotification.shown;
    expect(shown.title).toBe("tidy");
    expect(shown.options).toMatchObject({ body: "opus finished its turn", tag: "s1" });
    shown.onclick?.();
    expect(raise).toHaveBeenCalled();
    expect(onOpen).toHaveBeenCalledWith("t1");
    expect(shown.close).toHaveBeenCalled();
  });

  it("does not alert while the page has focus, or with alerts turned off", () => {
    FakeNotification.permission = "granted";
    localStorage.setItem("foundry.alerts", "on");
    const { rerender } = render(<Harness state="working" onOpen={() => {}} />);
    rerender(<Harness state="idle" onOpen={() => {}} />);
    expect(FakeNotification.shown).toHaveLength(0);

    fireEvent.click(screen.getByRole("button", { name: "Alerts on" }));
    expect(localStorage.getItem("foundry.alerts")).toBe("off");
    focused = false;
    rerender(<Harness state="working" onOpen={() => {}} />);
    rerender(<Harness state="waiting_for_you" onOpen={() => {}} />);
    expect(FakeNotification.shown).toHaveLength(0);
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
  it("tells a screen reader when a session needs you or finishes, whatever the alerts are set to", () => {
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
