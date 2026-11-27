import { screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Changes, Shipping } from "./api";
import { fleet, locate } from "./fleet";
import { headerFit, TerminalHeader } from "./TerminalHeader";
import { ME, sample, session, settings, terminal, workspace } from "./test-fixtures";
import { json, renderApp } from "./test-utils";

const dirty: Changes = {
  base: "main",
  commits: [],
  files: [{ path: "README.md", from: null, status: "modified", added: 2, removed: 0, uncommitted: true, patch: null }],
};
const quiet: Shipping = { branch: "say-hi", pushed: false, unpushed: 0, pr: null, unreachable: false };

/** A ready session of this machine in a workspace with a shell beside it, as the header sees it. */
function located() {
  const machines = fleet(
    [sample(ME, 1), workspace(ME, "w1", "rate limit failed logins"), terminal(ME, "t1", "w1"), session(ME, "s1", "w1", "t1", "idle"), terminal(ME, "t2", "w1")],
    settings({ machines: [{ id: ME, name: "studio", address: "" }] }),
  );
  return locate(machines, "t1")!;
}

/** Lays the header out `width` pixels wide: what it measures of itself, and what it hears when that changes. */
function layOut(width: number) {
  vi.stubGlobal(
    "ResizeObserver",
    class {
      constructor(private change: () => void) {}
      observe(element: Element) {
        element.getBoundingClientRect = () => ({ width }) as DOMRect;
        this.change();
      }
      disconnect() {}
    },
  );
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      if (url === "/api/machine") return json(200, { id: ME });
      if (url === "/api/workspaces/w1/changes") return json(200, dirty);
      if (url === "/api/workspaces/w1/pr") return json(200, quiet);
      throw new Error(`unexpected request ${url}`);
    }),
  );
}

async function header(width: number) {
  layOut(width);
  await renderApp(
    <TerminalHeader id="t1" located={located()} drawerOpen={false} onMenu={() => {}} onJump={() => {}} onChanges={() => {}} onSteps={() => {}} onFind={() => {}} onSplit={() => {}} />,
  );
  const bar = await screen.findByRole("banner", { name: "Open terminal" });
  // The workspace's name and the open terminal's tab come first, at every width.
  expect(within(bar).getByText("rate limit failed logins")).toBeTruthy();
  expect(within(bar).getByRole("tab", { selected: true }).textContent).toContain("opus");
  return bar;
}

afterEach(() => vi.unstubAllGlobals());

describe("headerFit", () => {
  it("gives the header's room out by how wide it is, and all of it while that is not known", () => {
    expect(headerFit(undefined)).toBe("wide");
    expect(headerFit(1400)).toBe("wide");
    expect(headerFit(900)).toBe("snug");
    expect(headerFit(608)).toBe("tight");
    expect(headerFit(358)).toBe("tight");
  });
});

describe("TerminalHeader", () => {
  it("shows everything when wide: the machine, the next steps and the buttons' words", async () => {
    const bar = await header(1400);

    expect(within(bar).getByText("studio")).toBeTruthy();
    expect(await within(bar).findByRole("group", { name: "Next for opus" })).toBeTruthy();
    expect(within(bar).getByRole("button", { name: "Steps" }).textContent).toBe("Steps");
    expect(within(bar).getByRole("button", { name: /^Changes/ }).querySelector(".sr-only")).toBeNull();
  });

  it("when snug, gives up the next steps and the machine, and its buttons keep their names but not their words", async () => {
    const bar = await header(900);

    expect(within(bar).queryByRole("group", { name: "Next for opus" })).toBeNull();
    expect(within(bar).queryByText("studio")).toBeNull();
    expect(within(bar).getByRole("button", { name: "Steps" }).querySelector(".sr-only")).toBeTruthy();
    expect(within(bar).getByRole("button", { name: /^Changes/ }).querySelector(".sr-only")?.textContent).toBe("Changes");
    expect(within(bar).getByRole("tablist").className.split(" ")).not.toContain("order-last");
  });

  it("when tight, folds changes and steps into More and puts the tabs on their own row", async () => {
    const bar = await header(608);

    expect(within(bar).queryByRole("button", { name: /^Changes/ })).toBeNull();
    expect(within(bar).queryByRole("button", { name: "Steps" })).toBeNull();
    expect(within(bar).getByRole("button", { name: "Find" })).toBeTruthy();
    expect(within(bar).getByRole("tablist").className.split(" ")).toContain("order-last");
  });
});
