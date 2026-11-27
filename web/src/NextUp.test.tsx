import { screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { NextUp } from "./NextUp";
import type { Needing } from "./needing";
import { renderApp } from "./test-utils";

const next: Needing = { terminal: "t4", workspace: "tidy cart styles", name: "opus", glyph: "waiting", label: "Needs you", now: { kind: "ask", text: "Run the integration tests against staging?" } };

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
    await renderApp(<NextUp next={{ ...next, glyph: "stuck", label: "Stuck", now: undefined }} others={1} />);
    const link = await screen.findByRole("link", { name: "Next that needs you: tidy cart styles / opus, 1 needs you" });
    expect(link.textContent).toContain("Stuck");
  });
});
