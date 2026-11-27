import { screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { NextUp } from "./NextUp";
import type { Needing } from "./needing";
import { renderApp } from "./test-utils";

const next: Needing = { terminal: "t4", workspace: "tidy cart styles", name: "opus", glyph: "waiting", label: "Needs you", now: { kind: "ask", text: "Run the integration tests against staging?" } };

describe("NextUp", () => {
  it("links to the next terminal that needs you, saying where it is, what it asks and how many need you", async () => {
    await renderApp(<NextUp next={next} others={3} answered={false} />);

    const link = await screen.findByRole("link", { name: "Next that needs you: tidy cart styles / opus, 3 more need you" });
    expect(link.getAttribute("href")).toBe("/terminals/t4");
    expect(link.title).toContain("Run the integration tests against staging?");
    expect(link.textContent).toContain("tidy cart styles");
    expect(link.textContent).toContain("3");
  });

  it("stays quiet while the open terminal still needs you, since that one comes first", async () => {
    await renderApp(<NextUp next={next} others={2} answered={false} />);
    expect((await screen.findByRole("link")).dataset.answered).toBeUndefined();
  });

  it("stands out once the open terminal no longer needs you, so the next one is the obvious move", async () => {
    await renderApp(<NextUp next={next} others={1} answered />);
    expect((await screen.findByRole("link")).dataset.answered).toBe("true");
  });

  it("says one needs you in the singular", async () => {
    await renderApp(<NextUp next={{ ...next, now: undefined }} others={1} answered />);
    const link = await screen.findByRole("link", { name: "Next that needs you: tidy cart styles / opus, 1 more needs you" });
    expect(link.title).toContain("Needs you");
  });
});
