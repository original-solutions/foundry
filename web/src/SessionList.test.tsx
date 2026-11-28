import { screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { fleet, withFactory } from "./fleet";
import { SessionList } from "./SessionList";
import { factoryStatus, ME, sample, session, settings, terminal, workspace } from "./test-fixtures";
import { renderApp } from "./test-utils";

describe("SessionList and the factory", () => {
  it("says what a held-up run is stuck on in place of its ended session's state, under needs you, with the whole reason on hover", async () => {
    const reason = "gate red: make check fails on main, see the gate log";
    const machines = withFactory(
      fleet([sample(ME, 1), workspace(ME, "w2", "fac/1027-2"), terminal(ME, "t2", "w2"), session(ME, "s2", "w2", "t2", "finished")], settings()),
      factoryStatus("p1", {}, { "o__r__1027__2": { kind: "stuck", workspaces: ["w2"], reason } }),
    );
    await renderApp(<SessionList machines={machines} docked onDock={vi.fn()} onPick={vi.fn()} />);
    const needs = screen.getByRole("group", { name: "Needs you" });
    const row = within(needs).getByRole("link", { name: `fac/1027-2 / opus: Stuck · ${reason}` });
    const label = within(row).getByText("Stuck · gate red: make check…");
    expect(label.getAttribute("title")).toBe(`Stuck · ${reason}`);
    expect(within(row).queryByText("Ended")).toBeNull();
  });
});
