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
});
