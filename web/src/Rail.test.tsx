import { fireEvent, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { fleet, withFactory } from "./fleet";
import { loadRail, saveRail } from "./collapsed";
import { Rail } from "./Rail";
import { factoryStatus, ME, OTHER, sample, session, settings, terminal, workspace } from "./test-fixtures";
import { renderApp } from "./test-utils";

function machines() {
  const records = [
    sample(ME, 1),
    workspace(ME, "w1", "fix login"),
    terminal(ME, "t1", "w1", "running"),
    session(ME, "s1", "w1", "t1", "working", { model: "opus" }),
    terminal(ME, "t2", "w1", "running"),
    workspace(ME, "w2", "many"),
    ...Array.from({ length: 8 }, (_, i) => terminal(ME, `m${i}`, "w2", "running")),
    workspace(OTHER, "w3", "on the laptop"),
    terminal(OTHER, "t3", "w3", "running"),
    session(OTHER, "s3", "w3", "t3", "waiting_for_you", { model: "opus" }),
  ];
  return fleet(records, settings({ machines: [{ id: ME, name: "studio", address: "" }, { id: OTHER, name: "laptop", address: "" }] }));
}

const handlers = () => ({ onExpand: vi.fn(), onJump: vi.fn(), onCreate: vi.fn() });

afterEach(() => localStorage.clear());

describe("Rail", () => {
  const rail = () => screen.getByRole("navigation", { name: "Workspaces" });
  const sessions = () => screen.getByRole("button", { name: /^Sessions/ });
  const list = () => screen.queryByRole("region", { name: "Sessions" });
  const labels = (group: string) => within(within(list()!).getByRole("group", { name: group })).getAllByRole("link").map((l) => l.getAttribute("aria-label"));

  it("shows no terminals of its own: one Sessions button carries how many need you", async () => {
    await renderApp(<Rail machines={machines()} {...handlers()} />);

    expect(within(rail()).queryAllByRole("link", { name: / \/ / })).toEqual([]);
    expect(sessions().getAttribute("aria-label")).toBe("Sessions: 1 needs you");
    expect(sessions().getAttribute("aria-expanded")).toBe("false");
    expect(sessions().textContent).toBe("1");
    expect(list()).toBeNull();
  });

  it("opens the sessions as one list grouped by state, most urgent first, each a link to its terminal", async () => {
    await renderApp(<Rail machines={machines()} selected="t2" {...handlers()} />);
    fireEvent.click(sessions());

    expect(sessions().getAttribute("aria-expanded")).toBe("true");
    expect(within(list()!).getAllByRole("group").map((g) => g.getAttribute("aria-label"))).toEqual(["Needs you", "Working", "Ready"]);
    expect(labels("Needs you")).toEqual(["on the laptop / opus: Needs you"]);
    expect(labels("Working")).toEqual(["fix login / opus: Working"]);
    expect(labels("Ready")).toHaveLength(9);
    const open = within(list()!).getByRole("link", { name: "fix login / shell: Open" });
    expect(open.getAttribute("href")).toBe("/terminals/t2");
    expect(open.getAttribute("aria-current")).toBe("true");
    // A row says which machine it is on when there is more than one.
    expect(within(list()!).getByRole("link", { name: "on the laptop / opus: Needs you" }).textContent).toMatch(/laptop/);
  });

  it("says a live factory run's stuck session is quiet, as the board does", async () => {
    const records = [sample(ME, 1), workspace(ME, "w1", "fac 162"), terminal(ME, "t1", "w1"), session(ME, "s1", "w1", "t1", "stuck")];
    const run = withFactory(fleet(records, settings()), factoryStatus("p1", { "original-solutions__foundry__162__1": ["w1"] }));
    await renderApp(<Rail machines={run} {...handlers()} />);
    fireEvent.click(sessions());

    expect(labels("Working")).toEqual(["fac 162 / opus: Quiet 10+ min"]);
  });

  it("closes the list when a session is picked, on Escape, or from its button", async () => {
    await renderApp(<Rail machines={machines()} {...handlers()} />);

    fireEvent.click(sessions());
    fireEvent.click(within(list()!).getByRole("link", { name: "fix login / opus: Working" }));
    expect(list()).toBeNull();

    fireEvent.click(sessions());
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "Escape" });
    expect(list()).toBeNull();
    expect(document.activeElement).toBe(sessions());

    fireEvent.click(sessions());
    fireEvent.click(sessions());
    expect(list()).toBeNull();
  });

  it("docks the list beside the rail, where a pick leaves it open, and keeps it docked next time", async () => {
    const { unmount } = await renderApp(<Rail machines={machines()} {...handlers()} />);
    fireEvent.click(sessions());
    fireEvent.click(within(list()!).getByRole("button", { name: "Dock beside the rail" }));

    fireEvent.click(within(list()!).getByRole("link", { name: "fix login / opus: Working" }));
    fireEvent.keyDown(document.body, { key: "Escape" });
    expect(list()).not.toBeNull();
    unmount();

    await renderApp(<Rail machines={machines()} {...handlers()} />);
    expect(list()).not.toBeNull();
    fireEvent.click(within(list()!).getByRole("button", { name: "Undock" }));
    fireEvent.click(within(list()!).getByRole("link", { name: "fix login / opus: Working" }));
    expect(list()).toBeNull();
  });

  it("walks the list with the arrows from the session that is open", async () => {
    await renderApp(<Rail machines={machines()} selected="t2" {...handlers()} />);
    fireEvent.click(sessions());

    const links = within(list()!).getAllByRole("link");
    expect(document.activeElement).toBe(links[2]);
    fireEvent.keyDown(links[2], { key: "ArrowUp" });
    expect(document.activeElement).toBe(links[1]);
    fireEvent.keyDown(links[1], { key: "End" });
    expect(document.activeElement).toBe(links.at(-1));
    fireEvent.keyDown(links.at(-1)!, { key: "Home" });
    expect(document.activeElement).toBe(links[0]);
  });

  it("links to the factory beside the board", async () => {
    await renderApp(<Rail machines={machines()} {...handlers()} />);
    expect(screen.getByRole("link", { name: "The factory" }).getAttribute("href")).toBe("/factory");
  });

  it("expands to the full list, jumps and starts a task from its buttons", async () => {
    const on = handlers();
    await renderApp(<Rail machines={machines()} {...on} />);

    fireEvent.click(screen.getByRole("button", { name: "Expand sidebar" }));
    fireEvent.click(screen.getByRole("button", { name: "Jump to" }));
    fireEvent.click(screen.getByRole("button", { name: "New task" }));
    expect(on.onExpand).toHaveBeenCalledOnce();
    expect(on.onJump).toHaveBeenCalledOnce();
    expect(on.onCreate).toHaveBeenCalledOnce();
    expect(screen.getByRole("link", { name: "The board" }).getAttribute("href")).toBe("/");
  });
});

describe("loadRail", () => {
  it("keeps the choice made, and with none collapses on a window too narrow for the list and a terminal", () => {
    expect(loadRail(900)).toBe(true);
    expect(loadRail(1440)).toBe(false);
    saveRail(true);
    expect(loadRail(1440)).toBe(true);
    saveRail(false);
    expect(loadRail(900)).toBe(false);
  });

  it("falls back to the width when storage is blocked or holds something else", () => {
    localStorage.setItem("foundry.rail", "maybe");
    expect(loadRail(900)).toBe(true);
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    expect(loadRail(1440)).toBe(false);
    vi.restoreAllMocks();
  });
});
