import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { fleet } from "./fleet";
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

    it("shows a 422's error the same way", async () => {
      mockFetch(json(422, { error: "a machine's name is at most 60 characters" }));
      await show();

      rename("x".repeat(61));
      fireEvent.keyDown(nameField(), { key: "Enter" });

      expect((await screen.findByRole("alert")).textContent).toBe("a machine's name is at most 60 characters");
      expect(nameField().value).toBe("x".repeat(61));
    });
  });
});
