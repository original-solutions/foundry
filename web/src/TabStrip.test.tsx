import { screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { fleet, withFactory } from "./fleet";
import { TabStrip } from "./TabStrip";
import { factoryStatus, ME, sample, session, settings, terminal, workspace } from "./test-fixtures";
import { renderApp } from "./test-utils";

describe("TabStrip and the factory", () => {
  it("says a held-up run's stage on its ended terminals' tabs, with the whole reason in the tooltip", async () => {
    const reason = "gate red: make check fails on main, see the gate log";
    const [machine] = withFactory(
      fleet([sample(ME, 1), workspace(ME, "w1", "fac/1027-2"), terminal(ME, "t1", "w1"), session(ME, "s1", "w1", "t1", "finished"), terminal(ME, "t2", "w1", "exited", { exit_code: 0 })], settings()),
      factoryStatus("p1", {}, { "o__r__1027__2": { kind: "stuck", workspaces: ["w1"], reason } }),
    );
    await renderApp(<TabStrip workspace={machine.workspaces[0]} machine={machine} active="t1" changed={new Set(["t2"])} />);
    const open = screen.getByRole("tab", { name: /^opus/ });
    expect(open.getAttribute("title")).toBe(`fac/1027-2 / opus: Stuck · ${reason}`);
    expect(screen.getByRole("tab", { name: /^shell/ }).getAttribute("title")).toBe(`fac/1027-2 / shell: Stuck · ${reason}`);
    // The open tab and the changed one say the label, cut to fit.
    expect(screen.getAllByText("Stuck · gate red: make check…")).toHaveLength(2);
    expect(screen.queryByText("Ended")).toBeNull();
  });
});
