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
  it("says a live factory run's stuck session is quiet, as the board does", async () => {
    const records = [sample(ME, 1), workspace(ME, "w1", "fac 162"), terminal(ME, "t1", "w1"), session(ME, "s1", "w1", "t1", "stuck")];
    const run = withFactory(fleet(records, settings()), factoryStatus("p1", { "original-solutions__foundry__162__1": ["w1"] }));
    await renderApp(<Rail machines={run} {...handlers()} />);
    const group = within(screen.getByRole("navigation", { name: "Workspaces" })).getByRole("group", { name: "fac 162" });
    expect(within(group).getByRole("link").getAttribute("aria-label")).toBe("fac 162 / opus: Quiet 10+ min");
  });

  it("links to the factory beside the board", async () => {
    await renderApp(<Rail machines={machines()} onExpand={() => {}} onJump={() => {}} onCreate={() => {}} />);
    expect(screen.getByRole("link", { name: "The factory" }).getAttribute("href")).toBe("/factory");
  });

  it("shows every workspace as its terminals' states, each a link named by workspace, terminal and state, the open one marked", async () => {
    await renderApp(<Rail machines={machines()} selected="t2" {...handlers()} />);

    const rail = screen.getByRole("navigation", { name: "Workspaces" });
    const login = within(rail).getByRole("group", { name: "fix login" });
    const links = within(login).getAllByRole("link");
    expect(links.map((l) => l.getAttribute("aria-label"))).toEqual(["fix login / opus: Working", "fix login / shell: Open"]);
    expect(links[1].getAttribute("aria-current")).toBe("true");
    expect(links[1].getAttribute("href")).toBe("/terminals/t2");
    // The workspace of the open terminal stands out from the others.
    expect(login.getAttribute("data-open")).toBe("true");
    expect(within(rail).getByRole("group", { name: "many" }).getAttribute("data-open")).toBeNull();
    // Another machine's workspaces are under its name, its state as plain as this one's.
    expect(within(within(rail).getByRole("group", { name: "on the laptop" })).getByRole("link").getAttribute("aria-label")).toBe("on the laptop / opus: Needs you");
    expect(within(rail).getByRole("link", { name: /^laptop/ }).getAttribute("href")).toBe("/machines");
  });

  it("shows six terminals of a workspace and counts the rest", async () => {
    await renderApp(<Rail machines={machines()} {...handlers()} />);

    const many = screen.getByRole("group", { name: "many" });
    expect(within(many).getAllByRole("link")).toHaveLength(6);
    expect(within(many).getByText("+2").getAttribute("title")).toBe("2 more in many");
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

  it("is one Tab stop whose arrows walk the terminals", async () => {
    await renderApp(<Rail machines={machines()} selected="t2" {...handlers()} />);

    const links = within(screen.getByRole("navigation", { name: "Workspaces" })).getAllByRole("link", { name: / \/ / });
    expect(links.filter((l) => l.tabIndex === 0).map((l) => l.getAttribute("href"))).toEqual(["/terminals/t2"]);
    links[1].focus();
    fireEvent.keyDown(links[1], { key: "ArrowDown" });
    expect(document.activeElement).toBe(links[2]);
    fireEvent.keyDown(links[2], { key: "ArrowUp" });
    expect(document.activeElement).toBe(links[1]);
    fireEvent.keyDown(links[1], { key: "End" });
    expect(document.activeElement).toBe(links.at(-1));
    fireEvent.keyDown(links.at(-1)!, { key: "Home" });
    expect(document.activeElement).toBe(links[0]);
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
