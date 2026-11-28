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

  describe("renaming", () => {
    const nameField = () => screen.getByRole<HTMLInputElement>("textbox", { name: "Machine name" });
    const rename = (to: string) => {
      fireEvent.click(within(card("studio")).getByRole("button", { name: "studio" }));
      fireEvent.change(nameField(), { target: { value: to } });
    };

    it("renames a machine in place: a click on its name, then Enter posts the name, trimmed, and closes the field on 204", async () => {
      const calls = mockFetch(new Response(null, { status: 204 }));
      await show();

      rename("  Tom's desk ");
      fireEvent.keyDown(nameField(), { key: "Enter" });

      await waitFor(() => expect(screen.queryByRole("textbox", { name: "Machine name" })).toBeNull());
      expect(calls).toEqual([{ url: `/api/machines/${ME}/rename`, method: "POST", body: { name: "Tom's desk" } }]);
    });

    it("opens the field on the machine's name, all of it selected", async () => {
      mockFetch();
      await show();

      fireEvent.click(within(card("studio")).getByRole("button", { name: "studio" }));

      expect(nameField().value).toBe("studio");
      expect(document.activeElement).toBe(nameField());
    });

    it("sends nothing on Escape and shows the name again", async () => {
      const calls = mockFetch();
      await show();

      rename("Tom's desk");
      fireEvent.keyDown(nameField(), { key: "Escape" });

      expect(screen.queryByRole("textbox", { name: "Machine name" })).toBeNull();
      expect(within(card("studio")).getByRole("button", { name: "studio" })).toBeDefined();
      expect(calls).toEqual([]);
    });

    it("keeps the field open, with its text, when it loses focus", async () => {
      const calls = mockFetch();
      await show();

      rename("Tom's desk");
      fireEvent.blur(nameField());

      expect(nameField().value).toBe("Tom's desk");
      expect(calls).toEqual([]);
    });

    it("sends nothing for the name it already has", async () => {
      const calls = mockFetch();
      await show();

      rename(" studio ");
      fireEvent.keyDown(nameField(), { key: "Enter" });

      expect(screen.queryByRole("textbox", { name: "Machine name" })).toBeNull();
      expect(calls).toEqual([]);
    });

    it("shows a 409's error under the field and keeps the text", async () => {
      mockFetch(json(409, { error: 'another machine is named "laptop"' }));
      await show();

      rename("Laptop");
      fireEvent.keyDown(nameField(), { key: "Enter" });

      expect((await screen.findByRole("alert")).textContent).toBe('another machine is named "laptop"');
      expect(nameField().value).toBe("Laptop");
    });

    it("shows the field busy while the rename is on its way, then takes edits again", async () => {
      mockFetch();
      const served = globalThis.fetch;
      let answer!: (response: Response) => void;
      vi.stubGlobal(
        "fetch",
        vi.fn((url: string, init?: RequestInit) => (url.endsWith("/rename") ? new Promise<Response>((r) => (answer = r)) : served(url, init))),
      );
      await show();

      rename("Laptop");
      fireEvent.keyDown(nameField(), { key: "Enter" });

      await waitFor(() => expect(nameField().getAttribute("aria-busy")).toBe("true"));
      expect(nameField().readOnly).toBe(true);
      answer(json(409, { error: 'another machine is named "laptop"' }));
      await screen.findByRole("alert");
      expect(nameField().getAttribute("aria-busy")).toBeNull();
      expect(nameField().readOnly).toBe(false);
    });

    it("keeps the card named, and its machine id on hover, while the field is open", async () => {
      mockFetch();
      await show();

      expect(within(card("studio")).getByTitle(new RegExp(ME))).toBeDefined();
      rename("Tom's desk");

      expect(within(card("studio")).getByRole("textbox", { name: "Machine name" })).toBeDefined();
    });

    it("offers no rename for a machine not in the machine list yet", async () => {
      mockFetch();
      const listed = settings({ machines: [{ id: ME, name: "studio", address: "" }] });
      await renderApp(<Machines machines={fleet(records, listed)} history={{}} here={ME} connected />);

      const unlisted = screen.getAllByRole("article").find((article) => !within(article).queryByText("This machine"))!;
      expect(within(unlisted).queryByRole("button")).toBeNull();
      expect(within(unlisted).getByTitle(OTHER)).toBeDefined();
      expect(within(card("studio")).getByRole("button", { name: "studio" })).toBeDefined();
    });

    it("shows a 422's error the same way", async () => {
      mockFetch(json(422, { error: "a machine's name is at most 60 characters" }));
      await show();

      rename("x".repeat(61));
      fireEvent.keyDown(nameField(), { key: "Enter" });

      expect((await screen.findByRole("alert")).textContent).toBe("a machine's name is at most 60 characters");
      expect(nameField().value).toBe("x".repeat(61));
    });
  });

  describe("limits", () => {
    const limited = settings({
      machines: twoMachines.machines,
      limits: { [ME]: { max_sessions: 4, max_load_per_core: 1.5, min_free_disk: 20_000_000_000 } },
    });
    const showLimited = (given = limited) => renderApp(<Machines machines={fleet(records, given)} history={{}} here={ME} connected />);
    const open = () => fireEvent.click(within(card("studio")).getByRole("button", { name: "Limits" }));
    const dialog = () => screen.getByRole("dialog", { name: "studio" });
    const sessionsField = () => within(dialog()).getByRole<HTMLInputElement>("spinbutton", { name: "Max sessions" });
    const loadField = () => within(dialog()).getByRole<HTMLInputElement>("spinbutton", { name: "Max load per core" });
    const diskField = () => within(dialog()).getByRole<HTMLInputElement>("spinbutton", { name: "Min free disk (GB)" });

    it("shows each machine's limits on its card, or that it has none", async () => {
      await showLimited();

      expect(within(card("studio")).getByText("At most 4 sessions · load 1.5 per core · 20 GB disk free")).toBeDefined();
      expect(within(card("laptop")).getByText("No limits")).toBeDefined();
    });

    it("opens a dialog titled with the machine's name, the fields filled from its limits", async () => {
      await showLimited();

      open();

      expect(sessionsField().value).toBe("4");
      expect(loadField().value).toBe("1.5");
      expect(diskField().value).toBe("20");
    });

    it("opens the fields empty for a machine with no limits and disables Clear", async () => {
      await showLimited();

      fireEvent.click(within(card("laptop")).getByRole("button", { name: "Limits" }));

      expect(within(screen.getByRole("dialog", { name: "laptop" })).getByRole<HTMLInputElement>("spinbutton", { name: "Max sessions" }).value).toBe("");
      expect(within(screen.getByRole("dialog", { name: "laptop" })).getByRole<HTMLButtonElement>("button", { name: "Clear" }).disabled).toBe(true);
    });

    it("Save posts the three fields as numbers, the disk in gigabytes, and closes on 204", async () => {
      const calls = mockFetch(new Response(null, { status: 204 }));
      await showLimited();

      open();
      fireEvent.change(sessionsField(), { target: { value: "6" } });
      fireEvent.change(loadField(), { target: { value: "0.8" } });
      fireEvent.change(diskField(), { target: { value: "4.35" } });
      fireEvent.click(within(dialog()).getByRole("button", { name: "Save" }));

      await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
      expect(calls).toEqual([
        { url: `/api/machines/${ME}/limits`, method: "POST", body: { max_sessions: 6, max_load_per_core: 0.8, min_free_disk_gb: 4.35 } },
      ]);
    });

    it("Save with the disk field left alone posts the stored disk limit exactly", async () => {
      const calls = mockFetch(new Response(null, { status: 204 }));
      await showLimited(settings({ machines: twoMachines.machines, limits: { [ME]: { max_sessions: 4, max_load_per_core: 1.5, min_free_disk: 4_355_500_000 } } }));

      open();
      expect(diskField().value).toBe("4.3555");
      fireEvent.change(sessionsField(), { target: { value: "5" } });
      fireEvent.click(within(dialog()).getByRole("button", { name: "Save" }));

      await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
      expect(calls[0].body).toEqual({ max_sessions: 5, max_load_per_core: 1.5, min_free_disk_gb: 4.3555 });
    });

    it("fills a disk limit of a single byte over a gigabyte exactly and saves it unchanged", async () => {
      const calls = mockFetch(new Response(null, { status: 204 }));
      await showLimited(settings({ machines: twoMachines.machines, limits: { [ME]: { max_sessions: 4, max_load_per_core: 1.5, min_free_disk: 1_000_000_001 } } }));

      open();
      expect(diskField().value).toBe("1.000000001");
      fireEvent.click(within(dialog()).getByRole("button", { name: "Save" }));

      await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
      expect(calls[0].body).toEqual({ max_sessions: 4, max_load_per_core: 1.5, min_free_disk_gb: 1.000000001 });
    });

    it("shows a disk limit on the card to three decimals", async () => {
      await showLimited(settings({ machines: twoMachines.machines, limits: { [ME]: { max_sessions: 4, max_load_per_core: 1.5, min_free_disk: 4_355_500_000 } } }));

      expect(within(card("studio")).getByText("At most 4 sessions · load 1.5 per core · 4.356 GB disk free")).toBeDefined();
    });

    it("says what the dialog does with each limit", async () => {
      await showLimited();

      open();

      expect(within(dialog()).getByText("Placement skips this machine while it has this many sessions or more, this load per core or more, or less free disk than this.")).toBeDefined();
    });

    it("sends an empty field as null, which the daemon refuses", async () => {
      const calls = mockFetch(json(422, { error: "the maximum sessions is a whole number from 1 to 4294967295" }));
      await showLimited();

      fireEvent.click(within(card("laptop")).getByRole("button", { name: "Limits" }));
      const laptop = within(screen.getByRole("dialog", { name: "laptop" }));
      fireEvent.change(laptop.getByRole("spinbutton", { name: "Max load per core" }), { target: { value: "1" } });
      fireEvent.click(laptop.getByRole("button", { name: "Save" }));

      await screen.findByRole("alert");
      expect(calls).toEqual([
        { url: `/api/machines/${OTHER}/limits`, method: "POST", body: { max_sessions: null, max_load_per_core: 1, min_free_disk_gb: null } },
      ]);
    });

    it("Clear posts to the clear route and closes on 204", async () => {
      const calls = mockFetch(new Response(null, { status: 204 }));
      await showLimited();

      open();
      fireEvent.click(within(dialog()).getByRole("button", { name: "Clear" }));

      await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
      expect(calls).toEqual([{ url: `/api/machines/${ME}/limits/clear`, method: "POST", body: undefined }]);
    });

    it("shows a 422's error in the dialog and keeps the values", async () => {
      mockFetch(json(422, { error: "the maximum load per core is a number above 0" }));
      await showLimited();

      open();
      fireEvent.change(loadField(), { target: { value: "0" } });
      fireEvent.click(within(dialog()).getByRole("button", { name: "Save" }));

      expect((await screen.findByRole("alert")).textContent).toBe("the maximum load per core is a number above 0");
      expect(loadField().value).toBe("0");
      expect(sessionsField().value).toBe("4");
      expect(diskField().value).toBe("20");
    });

    it("fills the fields from the current limits each time it opens", async () => {
      mockFetch(json(422, { error: "no" }));
      await showLimited();

      open();
      fireEvent.change(sessionsField(), { target: { value: "9" } });
      fireEvent.keyDown(dialog(), { key: "Escape" });
      await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
      open();

      expect(sessionsField().value).toBe("4");
    });

    it("offers no Limits for a machine not in the machine list yet", async () => {
      mockFetch();
      const listed = settings({ machines: [{ id: ME, name: "studio", address: "" }] });
      await showLimited(listed);

      const unlisted = screen.getAllByRole("article").find((article) => !within(article).queryByText("This machine"))!;
      expect(within(unlisted).queryByRole("button", { name: "Limits" })).toBeNull();
      expect(within(card("studio")).getByRole("button", { name: "Limits" })).toBeDefined();
    });
  });

  describe("removing a machine", () => {
    const confirmText = "laptop will be cut off from every machine and cannot join again under its current identity. Its workspaces stay on its disk.";
    const showListed = (given = twoMachines) => renderApp(<Machines machines={fleet(records, given)} history={{}} here={ME} connected />);
    const remove = () => fireEvent.click(within(card("laptop")).getByRole("button", { name: "Remove from fleet" }));

    it("is absent on this machine's card and on a card the machine list does not hold", async () => {
      await showListed(settings({ machines: [{ id: ME, name: "studio", address: "" }] }));

      expect(within(card("studio")).queryByRole("button", { name: "Remove from fleet" })).toBeNull();
      const unlisted = screen.getAllByRole("article").find((article) => !within(article).queryByText("This machine"))!;
      expect(within(unlisted).queryByRole("button", { name: "Remove from fleet" })).toBeNull();
    });

    it("offers no Remove on any card while this machine is not known yet", async () => {
      await renderApp(<Machines machines={fleet(records, twoMachines)} history={{}} connected />);

      expect(screen.getAllByRole("article")).toHaveLength(2);
      expect(screen.queryByRole("button", { name: "Remove from fleet" })).toBeNull();
    });

    it("asks first, with the name, and posts nothing until confirmed", async () => {
      const calls = mockFetch();
      await showListed();

      expect(within(card("studio")).queryByRole("button", { name: "Remove from fleet" })).toBeNull();
      remove();

      expect(within(screen.getByRole("dialog")).getByText(confirmText)).toBeDefined();
      expect(calls).toEqual([]);
    });

    it("posts to the remove route on confirm and closes on 204", async () => {
      const calls = mockFetch(new Response(null, { status: 204 }));
      await showListed();

      remove();
      fireEvent.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Remove from fleet" }));

      await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
      expect(calls).toEqual([{ url: `/api/machines/${OTHER}/remove`, method: "POST", body: undefined }]);
    });

    it("shows a 409's error in the dialog and keeps it open", async () => {
      mockFetch(json(409, { error: "remove a machine from another machine's UI" }));
      await showListed();

      remove();
      fireEvent.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Remove from fleet" }));

      expect((await screen.findByRole("alert")).textContent).toBe("remove a machine from another machine's UI");
      expect(screen.getByRole("dialog")).toBeDefined();
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

    it("keeps both moves disabled after a 204 until the new order arrives", async () => {
      const calls = mockFetch(new Response(null, { status: 204 }));
      const studioFirst = settings({ machines: laptopFirst.machines, priority: [ME, OTHER] });
      function Board() {
        const [held, setHeld] = useState(laptopFirst);
        return (
          <>
            <button type="button" onClick={() => setHeld(studioFirst)}>
              deliver
            </button>
            <button type="button" onClick={() => setHeld(laptopFirst)}>
              deliver the old order
            </button>
            <Machines machines={fleet(records, held)} history={{}} here={ME} connected />
          </>
        );
      }
      await renderApp(<Board />);
      const moves = () => screen.getAllByRole<HTMLButtonElement>("button", { name: /in the coordinator order/ });

      fireEvent.click(within(card("laptop")).getByRole("button", { name: "Move laptop down in the coordinator order" }));
      await waitFor(() => expect(calls).toHaveLength(1));
      await act(async () => {});

      expect(moves().every((b) => b.disabled)).toBe(true);

      fireEvent.click(screen.getByRole("button", { name: "deliver" }));

      await waitFor(() => expect(names()).toEqual(["studio", "laptop"]));
      expect(within(card("studio")).getByRole<HTMLButtonElement>("button", { name: "Move studio down in the coordinator order" }).disabled).toBe(false);
      expect(calls).toHaveLength(1);

      // Another machine moves it back: nothing is pending, so the moves are on.
      fireEvent.click(screen.getByRole("button", { name: "deliver the old order" }));

      await waitFor(() => expect(names()).toEqual(["laptop", "studio"]));
      expect(moves().filter((b) => b.disabled).map((b) => b.getAttribute("aria-label"))).toEqual([
        "Move laptop up in the coordinator order",
        "Move studio down in the coordinator order",
      ]);
    });

    it("posts the whole new order when a machine moves", async () => {
      const calls = mockFetch(new Response(null, { status: 204 }));
      await order();

      fireEvent.click(within(card("laptop")).getByRole("button", { name: "Move laptop down in the coordinator order" }));

      await waitFor(() => expect(calls).toEqual([{ url: "/api/machines/priority", method: "POST", body: { order: [ME, OTHER] } }]));
    });

    // With three machines a wrong neighbour no longer looks right: each move swaps with the next one.
    it.each([
      ["laptop", "up", ["machine-2", "machine-1", "machine-3"]],
      ["desk", "up", ["machine-1", "machine-3", "machine-2"]],
      ["studio", "down", ["machine-2", "machine-1", "machine-3"]],
      ["laptop", "down", ["machine-1", "machine-3", "machine-2"]],
    ])("posts the order with %s moved %s one place among three machines", async (name, way, posted) => {
      const calls = mockFetch(new Response(null, { status: 204 }));
      await order(
        settings({
          machines: [
            { id: ME, name: "studio", address: "" },
            { id: OTHER, name: "laptop", address: "" },
            { id: "machine-3", name: "desk", address: "" },
          ],
          priority: [ME, OTHER, "machine-3"],
        }),
      );
      expect(names()).toEqual(["studio", "laptop", "desk"]);

      fireEvent.click(within(card(name)).getByRole("button", { name: `Move ${name} ${way} in the coordinator order` }));

      await waitFor(() => expect(calls).toEqual([{ url: "/api/machines/priority", method: "POST", body: { order: posted } }]));
    });

    it("disables the top machine's up and the bottom machine's down", async () => {
      await order();

      expect((within(card("laptop")).getByRole("button", { name: "Move laptop up in the coordinator order" }) as HTMLButtonElement).disabled).toBe(true);
      expect((within(card("studio")).getByRole("button", { name: "Move studio down in the coordinator order" }) as HTMLButtonElement).disabled).toBe(true);
      expect((within(card("studio")).getByRole("button", { name: "Move studio up in the coordinator order" }) as HTMLButtonElement).disabled).toBe(false);
    });

    it("shows a refusal's error in an alert", async () => {
      mockFetch(json(422, { error: "no machine \"x\" in the machine list" }));
      await order();

      fireEvent.click(within(card("studio")).getByRole("button", { name: "Move studio up in the coordinator order" }));

      expect((await screen.findByRole("alert")).textContent).toBe('no machine "x" in the machine list');
      expect(within(card("studio")).getByRole<HTMLButtonElement>("button", { name: "Move studio up in the coordinator order" }).disabled).toBe(false);
    });

    it("offers no move when fewer than two machines are listed", async () => {
      await order(settings({ machines: [{ id: ME, name: "studio", address: "" }], priority: [ME] }));

      expect(screen.queryByRole("button", { name: /in the coordinator order/ })).toBeNull();
    });
  });

  describe("adding a machine", () => {
    const added = { code: "K7QX3M9PRT", command: "foundryd join 100.101.102.103 K7QX3M9PRT", expires_in_secs: 600 };
    const nameField = () => screen.getByRole<HTMLInputElement>("textbox", { name: "Name" });
    const add = (name: string) => {
      fireEvent.click(screen.getByRole("button", { name: "Add machine" }));
      fireEvent.change(nameField(), { target: { value: name } });
      fireEvent.click(screen.getByRole("button", { name: "Add" }));
    };

    it("posts the name typed and shows the command to run on the new machine, which Copy puts on the clipboard", async () => {
      const calls = mockFetch(json(201, added));
      const writeText = vi.fn(async () => {});
      vi.stubGlobal("navigator", { ...navigator, clipboard: { writeText } });
      await show();

      add(" trilogy-mac ");
      expect(nameField().placeholder).toBe("Tailscale name, e.g. trilogy-mac");

      const command = await screen.findByText(added.command);
      expect(command.closest("pre, code")).not.toBeNull();
      expect(screen.getByText(/expires in 10 minutes/)).toBeDefined();
      expect(calls).toEqual([{ url: "/api/machines/add", method: "POST", body: { name: "trilogy-mac" } }]);
      fireEvent.click(screen.getByRole("button", { name: "Copy" }));
      await waitFor(() => expect(writeText).toHaveBeenCalledWith(added.command));
    });

    it("shows a 422's error in the dialog and keeps the text", async () => {
      mockFetch(json(422, { error: '"trilogy-mac" is not on Tailscale: it resolves to no Tailscale address' }));
      await show();

      add("trilogy-mac");

      expect((await screen.findByRole("alert")).textContent).toBe('"trilogy-mac" is not on Tailscale: it resolves to no Tailscale address');
      expect(nameField().value).toBe("trilogy-mac");
    });

    it("shows a 409's error the same way", async () => {
      mockFetch(json(409, { error: 'a machine named "laptop" is already in the fleet' }));
      await show();

      add("Laptop");

      expect((await screen.findByRole("alert")).textContent).toBe('a machine named "laptop" is already in the fleet');
      expect(nameField().value).toBe("Laptop");
    });

    it("says the machine joined once it shows in the settings, and its card appears", async () => {
      mockFetch(json(201, added));
      let setMachines!: (machines: MachineView[]) => void;
      function Live() {
        const [machines, set] = useState(fleet(records, twoMachines));
        setMachines = set;
        return <Machines machines={machines} history={{}} here={ME} connected />;
      }
      await renderApp(<Live />);

      add("trilogy-mac");
      await screen.findByText(added.command);
      expect(screen.queryByText("trilogy-mac joined")).toBeNull();
      const joined = settings({ machines: [...twoMachines.machines, { id: "m-3", name: "trilogy-mac", address: "100.64.0.3" }] });
      act(() => setMachines(fleet(records, joined)));

      expect((await screen.findByRole("status")).textContent).toBe("trilogy-mac joined");
      // Behind the dialog, which hides the rest of the page from the accessibility tree while open.
      expect(screen.getByRole("article", { name: "trilogy-mac", hidden: true })).toBeDefined();
    });

    it("says the machine joined when the listed name differs from the typed one only in case", async () => {
      mockFetch(json(201, added));
      let setMachines!: (machines: MachineView[]) => void;
      function Live() {
        const [machines, set] = useState(fleet(records, twoMachines));
        setMachines = set;
        return <Machines machines={machines} history={{}} here={ME} connected />;
      }
      await renderApp(<Live />);

      add("Work-Mac");
      await screen.findByText(added.command);
      const joined = settings({ machines: [...twoMachines.machines, { id: "m-3", name: "work-mac", address: "100.64.0.3" }] });
      act(() => setMachines(fleet(records, joined)));

      expect((await screen.findByRole("status")).textContent).toMatch(/joined$/);
    });

    it("says the machine joined when it lists itself under its own name at the address the typed name resolved to", async () => {
      mockFetch(json(201, { ...added, address: "100.64.0.3" }));
      let setMachines!: (machines: MachineView[]) => void;
      function Live() {
        const [machines, set] = useState(fleet(records, twoMachines));
        setMachines = set;
        return <Machines machines={machines} history={{}} here={ME} connected />;
      }
      await renderApp(<Live />);

      add("trilogy-mac.tail459612.ts.net");
      await screen.findByText(added.command);
      const joined = settings({ machines: [...twoMachines.machines, { id: "m-3", name: "trilogy-mac", address: "100.64.0.3" }] });
      act(() => setMachines(fleet(records, joined)));

      expect((await screen.findByRole("status")).textContent).toBe("trilogy-mac.tail459612.ts.net joined");
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
