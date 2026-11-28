import { renderHook, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { NextUp, useOpenStanding } from "./NextUp";
import type { Needing } from "./needing";
import { renderApp } from "./test-utils";

const next: Needing = { terminal: "t4", workspace: "tidy cart styles", name: "opus", glyph: "waiting", label: "Needs you", fullLabel: "Needs you", now: { kind: "ask", text: "Run the integration tests against staging?" } };

describe("NextUp", () => {
  it("from a view with no terminal open, leads to the first that needs you, saying where it is, what it asks and how many need you", async () => {
    await renderApp(<NextUp next={next} others={3} />);

    const link = await screen.findByRole("link", { name: "Next that needs you: tidy cart styles / opus, 3 need you" });
    expect(link.getAttribute("href")).toBe("/terminals/t4");
    expect(link.textContent).toContain("tidy cart styles");
    expect(link.textContent).toContain("Run the integration tests against staging?");
    expect(link.textContent).toContain("3 need you");
    expect(link.title).toContain("J");
    expect(link.dataset.answered).toBeUndefined();
  });

  it("stays quiet while the open terminal still needs you, since that one comes first, and counts the others as more", async () => {
    await renderApp(<NextUp next={next} others={2} open="needing" />);
    const link = await screen.findByRole("link", { name: "Next that needs you: tidy cart styles / opus, 2 more need you" });
    expect(link.textContent).toContain("2 more need you");
    expect(link.dataset.answered).toBeUndefined();
  });

  it("stands out once the open terminal no longer needs you, so the next one is the obvious move", async () => {
    await renderApp(<NextUp next={next} others={1} open="answered" />);
    const link = await screen.findByRole("link", { name: "Next that needs you: tidy cart styles / opus, 1 needs you" });
    expect(link.dataset.answered).toBe("true");
  });

  it("says what state it is in when it asks nothing", async () => {
    await renderApp(<NextUp next={{ ...next, glyph: "stuck", label: "Stuck", fullLabel: "Stuck", now: undefined }} others={1} />);
    const link = await screen.findByRole("link", { name: "Next that needs you: tidy cart styles / opus, 1 needs you" });
    expect(link.textContent).toContain("Stuck");
  });

  it("says what a held-up factory run is stuck on, its whole reason, since the strip has the room and cuts it itself", async () => {
    const held = { ...next, glyph: "done" as const, label: "Stuck · gate red: make check…", fullLabel: "Stuck · gate red: make check fails on main, see the gate log", now: undefined };
    await renderApp(<NextUp next={held} others={1} />);
    const link = await screen.findByRole("link", { name: "Next that needs you: tidy cart styles / opus, 1 needs you" });
    expect(link.textContent).toContain("Stuck · gate red: make check fails on main, see the gate log");
    expect(link.title).toContain("Stuck · gate red: make check fails on main, see the gate log ·");
  });
});

describe("useOpenStanding", () => {
  const standing = (terminal?: string, needs = false) => renderHook(({ terminal, needs }) => useOpenStanding(terminal, needs), { initialProps: { terminal, needs } });

  it("is nothing with no terminal open, and needing while the open one needs you", () => {
    expect(standing().result.current).toBeUndefined();
    expect(standing("t1", true).result.current).toBe("needing");
  });

  it("is nothing over a terminal that never needed you, so the strip does not glow as if you had answered", () => {
    expect(standing("t1", false).result.current).toBeUndefined();
  });

  it("is answered once the open terminal stops needing you, and not again after another terminal is opened", () => {
    const hook = standing("t1", true);
    hook.rerender({ terminal: "t1", needs: false });
    expect(hook.result.current).toBe("answered");
    hook.rerender({ terminal: "t2", needs: false });
    expect(hook.result.current).toBeUndefined();
    hook.rerender({ terminal: "t1", needs: false });
    expect(hook.result.current).toBeUndefined();
  });
});
