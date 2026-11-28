import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { fleet, type MachineView } from "./fleet";
import { Machines } from "./Machines";
import { remember } from "./samples";
import { ME, OTHER, sample, session, settings, terminal, workspace } from "./test-fixtures";
import { json, mockFetch, renderApp } from "./test-utils";

const twoMachines = settings({
  machines: [
    { id: ME, name: "studio", address: "" },
    { id: OTHER, name: "laptop", address: "" },
  ],
});

const records = [
  sample(ME, 2, { load: 0.5, mem: 16_300_000_000, disk: 26_800_000_000, sessions: 3 }),
  workspace(ME, "w1", "fix login"),
  terminal(ME, "t1", "w1"),
  session(ME, "s1", "w1", "t1", "idle"),
  terminal(ME, "t2", "w1"),
  session(ME, "s2", "w1", "t2", "waiting_for_you"),
  workspace(ME, "w2", "tidy readme"),
  terminal(ME, "t3", "w2"),
  session(ME, "s3", "w2", "t3", "working"),
  sample(OTHER, 9, { load: 1.4 }),
  workspace(OTHER, "w3", "far away"),
  terminal(OTHER, "t4", "w3", "unreachable"),
  session(OTHER, "s4", "w3", "t4", "unreachable"),
  terminal(OTHER, "t5", "w3", "unreachable"),
];

function show(extra: { connected?: boolean; here?: string } = {}) {
  const now = Date.now();
  let history = remember({}, [sample(ME, 1, { load: 0.2 }), sample(OTHER, 9, { load: 1.4 })], now - 10_000);
  history = remember(history, [sample(ME, 2, { load: 0.5 })], now - 4_000);
  return renderApp(<Machines machines={fleet(records, twoMachines)} history={history} here={extra.here ?? ME} connected={extra.connected ?? true} />);
}

const card = (name: string) => screen.getByRole("article", { name });

afterEach(() => vi.unstubAllGlobals());

describe("Machines", () => {
  it("shows every machine as a card, this one marked, with its live numbers and when it was last heard", async () => {
    await show();
    expect(screen.getByRole("region", { name: "Machines" })).toBeDefined();
    const studio = card("studio");
    expect(within(studio).getByText("This machine")).toBeDefined();
    expect(within(studio).getByText("online").getAttribute("class")).not.toMatch(/emerald/);
    expect(within(studio).getByText(/heard 4 s ago/)).toBeDefined();
    // A machine at rest is grey: its load (0.5 per core) takes a colour only near or past full.
    expect(within(studio).getByText("0.50").getAttribute("class")).toContain("text-neutral-");
    expect(within(studio).getByRole("meter", { name: "Load per core" }).getAttribute("aria-valuenow")).toBe("0.5");
    expect(within(studio).getByText("16.3 GB")).toBeDefined();
    expect(within(studio).getByText("26.8 GB")).toBeDefined();
    expect(within(card("laptop")).queryByText("This machine")).toBeNull();
  });

  it("shows the version each machine runs and the release waiting on it, or that its version is unknown", async () => {
    const withVersion = { ...records[0], body: { ...records[0].body, version: "0.9.0", update_ready: "v0.9.1" } } as (typeof records)[number];
    await renderApp(<Machines machines={fleet([withVersion, ...records.slice(1)], twoMachines)} history={{}} here={ME} connected />);
    expect(within(card("studio")).getByText("0.9.0")).toBeDefined();
    expect(within(card("studio")).getByText(/v0\.9\.1 waiting/)).toBeDefined();
    expect(within(card("laptop")).getByText(/version unknown/i)).toBeDefined();
    expect(within(card("laptop")).queryByText(/waiting/)).toBeNull();
  });

  it("draws the load since the page opened, up to ten minutes, as a line saying its range", async () => {
    await show();
    const line = within(card("studio")).getByRole("img", { name: /load per core, last 1 min/i });
    expect(line.getAttribute("aria-label")).toMatch(/0\.20 to 0\.50/);
    expect(line.querySelector("polyline")?.getAttribute("points")?.split(" ")).toHaveLength(2);
  });

  it("counts the machine's terminals by state, the ones that need you first", async () => {
    await show();
    const states = within(card("studio")).getByRole("list", { name: "Terminals by state" });
    expect(within(states).getAllByRole("listitem").map((li) => li.textContent)).toEqual(["1 needs you", "1 working", "1 ready"]);
  });

  it("lists the machine's workspaces, each opening its most urgent terminal", async () => {
    await show();
    const link = within(card("studio")).getByRole("link", { name: /fix login/ });
    expect(link.getAttribute("href")).toBe("/terminals/t2");
  });

  it("says an offline machine's terminals are unreachable until it is heard from, and its numbers are its last", async () => {
    await show();
    const laptop = card("laptop");
    expect(within(laptop).getByText("offline")).toBeDefined();
    expect(within(laptop).getByText(/2 terminals unreachable until it is heard from/)).toBeDefined();
    expect(within(laptop).getByText(/last known/i)).toBeDefined();
    expect(within(laptop).queryByText(/heard .* ago/)).toBeNull();
  });

  it("says the numbers stopped when the page lost the daemon", async () => {
    await show({ connected: false });
    expect(screen.getByText(/disconnected/i)).toBeDefined();
  });

  describe("settings", () => {
    const limited = settings({
      machines: twoMachines.machines,
      limits: { [ME]: { max_sessions: 4, max_load_per_core: 1.5, min_free_disk: 4_355_500_000 } },
    });

    it("gives each card a Settings link to its machine's page", async () => {
      await show();

      expect(within(card("studio")).getByRole("link", { name: "Settings" }).getAttribute("href")).toBe(`/settings/machines/${ME}`);
      expect(within(card("laptop")).getByRole("link", { name: "Settings" }).getAttribute("href")).toBe(`/settings/machines/${OTHER}`);
    });

    it("has no configuration controls: no limits, moves, remove, rename or Add machine", async () => {
      await renderApp(<Machines machines={fleet(records, limited)} history={{}} here={ME} connected onCreate={() => {}} />);

      expect(screen.queryByRole("button", { name: "Limits" })).toBeNull();
      expect(screen.queryByRole("button", { name: /in the coordinator order/ })).toBeNull();
      expect(screen.queryByRole("button", { name: "Remove from fleet" })).toBeNull();
      expect(screen.queryByRole("button", { name: "Add machine" })).toBeNull();
      expect(within(card("studio")).queryByRole("button", { name: "studio" })).toBeNull();
      expect(within(card("studio")).getByRole("heading", { name: "studio" })).toBeDefined();
    });

    it("keeps the status, the version and starting a task on each card", async () => {
      await renderApp(<Machines machines={fleet(records, limited)} history={{}} here={ME} connected onCreate={() => {}} />);

      expect(within(card("studio")).getByText("online")).toBeDefined();
      expect(within(card("studio")).getByText("Version unknown")).toBeDefined();
      expect(within(card("studio")).getByRole("button", { name: /new task/i })).toBeDefined();
    });

    it("shows each machine's limits on its card, the disk to three decimals, or that it has none", async () => {
      await renderApp(<Machines machines={fleet(records, limited)} history={{}} here={ME} connected />);

      expect(within(card("studio")).getByText("At most 4 sessions · load 1.5 per core · 4.356 GB disk free")).toBeDefined();
      expect(within(card("laptop")).getByText("No limits")).toBeDefined();
    });
  });

  describe("the coordinator order", () => {
    const laptopFirst = settings({
      machines: [
        { id: ME, name: "studio", address: "" },
        { id: OTHER, name: "laptop", address: "" },
      ],
      priority: [OTHER, ME],
    });
    const order = (given = laptopFirst, online?: string[]) => renderApp(<Machines machines={fleet(records, given, {}, online)} history={{}} here={ME} connected />);
    const names = () => screen.getAllByRole("article").map((a) => a.getAttribute("aria-label"));

    it("lists machines in priority order, listed ones before the rest", async () => {
      await order();

      expect(names()).toEqual(["laptop", "studio"]);
    });

    it("marks the machine that holds the coordinator role", async () => {
      await order(laptopFirst, [ME, OTHER]);

      expect(within(card("laptop")).getByText("Coordinator")).toBeDefined();
      expect(within(card("studio")).queryByText("Coordinator")).toBeNull();
      expect(within(card("laptop")).getByText("Coordinator order 1 of 2")).toBeDefined();
      expect(within(card("studio")).getByText("Coordinator order 2 of 2")).toBeDefined();
    });

    it("puts the badge on the machine that holds the role, not on the first of the order", async () => {
      await order(laptopFirst, [ME]);

      expect(within(card("laptop")).getByText("Coordinator order 1 of 2")).toBeDefined();
      expect(within(card("laptop")).queryByText("Coordinator")).toBeNull();
      expect(within(card("studio")).getByText("Coordinator")).toBeDefined();
    });
  });

  describe("room for the next task", () => {
    /** Both machines online and sampled: studio at 0.5 per core, laptop at 0.3. */
    const bothOnline = [
      sample(ME, 2, { load: 0.5, sessions: 3 }),
      workspace(ME, "w1", "fix login"),
      terminal(ME, "t1", "w1"),
      session(ME, "s1", "w1", "t1", "idle"),
      sample(OTHER, 9, { load: 0.3, sessions: 1 }),
    ];
    const showRoom = async (given = twoMachines, online = [ME, OTHER], onCreate = vi.fn()) => {
      await renderApp(<Machines machines={fleet(bothOnline, given, {}, online)} history={{}} here={ME} connected onCreate={onCreate} />);
      return onCreate;
    };
    const head = () => screen.getByRole("region", { name: "Next task" });

    it("names the machine Foundry would choose, and marks its card", async () => {
      await showRoom();
      expect(head().textContent).toMatch(/laptop takes the next task/);
      expect(head().textContent).toMatch(/2 of 2 machines have room/);
      expect(within(card("laptop")).getByText("Foundry's pick")).toBeDefined();
      expect(within(card("studio")).getByText("Has room")).toBeDefined();
    });

    it("says which limits keep a machine from the next task, and that a task would wait when none has room", async () => {
      const limited = settings({
        machines: twoMachines.machines,
        limits: {
          [ME]: { max_sessions: 3, max_load_per_core: 1.5, min_free_disk: 1 },
          [OTHER]: { max_sessions: 9, max_load_per_core: 0.25, min_free_disk: 1 },
        },
      });
      await showRoom(limited);
      expect(within(card("studio")).getByText("Full").parentElement?.textContent).toMatch(/sessions 3 at limit 3/);
      expect(within(card("laptop")).getByText("Full").parentElement?.textContent).toMatch(/load 0\.3 over limit 0\.25/);
      expect(head().textContent).toMatch(/No machine has room/);
      expect(within(head()).getAllByRole("listitem").map((li) => li.textContent)).toEqual(["studio: sessions 3 at limit 3", "laptop: load 0.3 over limit 0.25"]);
    });

    it("starts a task where Foundry chooses from the head, and on one machine from its card", async () => {
      const onCreate = await showRoom();
      fireEvent.click(within(head()).getByRole("button", { name: "New task, Foundry chooses the machine" }));
      expect(onCreate).toHaveBeenLastCalledWith("choose");
      fireEvent.click(within(card("studio")).getByRole("button", { name: "New task on studio" }));
      // This machine is the task box's default, named by no id.
      expect(onCreate).toHaveBeenLastCalledWith("");
      fireEvent.click(within(card("laptop")).getByRole("button", { name: "New task on laptop" }));
      expect(onCreate).toHaveBeenLastCalledWith(OTHER);
    });

    it("offers no task on an offline machine, and says nothing of its room beyond that it is offline", async () => {
      await showRoom(twoMachines, [ME]);
      expect(within(card("laptop")).queryByRole("button", { name: /New task/ })).toBeNull();
      expect(within(card("laptop")).queryByText("Has room")).toBeNull();
      expect(within(card("studio")).getByText("Foundry's pick")).toBeDefined();
      expect(head().textContent).toMatch(/1 of 2 machines has room/);
    });

    it("has no head and no pick for a fleet of one machine, whose card still starts a task", async () => {
      const onCreate = vi.fn();
      const one = settings({ machines: [{ id: ME, name: "studio", address: "" }] });
      await renderApp(<Machines machines={fleet(bothOnline.slice(0, 4), one, {}, [ME])} history={{}} here={ME} connected onCreate={onCreate} />);
      expect(screen.queryByRole("region", { name: "Next task" })).toBeNull();
      expect(within(card("studio")).queryByText("Foundry's pick")).toBeNull();
      fireEvent.click(within(card("studio")).getByRole("button", { name: "New task on studio" }));
      expect(onCreate).toHaveBeenCalledWith("");
    });
  });
});
