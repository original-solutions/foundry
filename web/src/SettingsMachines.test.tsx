import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AnyRecord, MachineSample, SharedSettings } from "./api";
import { emptyState, fleet } from "./fleet";
import { SettingsMachines } from "./SettingsMachines";
import { forgetInstalls } from "./UpdateChecklist";
import { ME, OTHER, sample, settings } from "./test-fixtures";
import { json, mockFetch, pageProps, renderApp } from "./test-utils";

afterEach(() => {
  vi.unstubAllGlobals();
  forgetInstalls();
});

const laptopFirst = settings({
  machines: [
    { id: ME, name: "studio", address: "" },
    { id: OTHER, name: "laptop", address: "" },
  ],
  priority: [OTHER, ME],
});

/** A sample of `owner` with the update fields given. */
function at(owner: string, version: number, update: Partial<Pick<MachineSample, "version" | "update_ready">> = {}): AnyRecord {
  const r = sample(owner, version) as AnyRecord & { kind: "machine_sample" };
  return { ...r, body: { ...r.body, ...update } };
}

const records = [at(ME, 1), at(OTHER, 1)];

function show(given: SharedSettings = laptopFirst, online?: string[], extra: AnyRecord[] = records) {
  const machines = fleet(extra, given, {}, online);
  return renderApp(<SettingsMachines {...pageProps({ state: { ...emptyState, synced: true, settings: given }, machines, here: ME })} />);
}

/** Shows the page over settings the test can change, as the events socket would. */
async function live(start: SharedSettings = laptopFirst) {
  let change: (next: SharedSettings) => void = () => {};
  function Live() {
    const [given, setGiven] = useState(start);
    change = setGiven;
    return <SettingsMachines {...pageProps({ state: { ...emptyState, synced: true, settings: given }, machines: fleet(records, given), here: ME })} />;
  }
  await renderApp(<Live />);
  return (next: SharedSettings) => act(() => change(next));
}

const order = () => screen.getByRole("list", { name: "Coordinator order" });
const names = () => within(order()).getAllByRole("listitem").map((li) => li.getAttribute("aria-label"));
const entry = (name: string) => within(order()).getByRole("listitem", { name });
const moveButton = (name: string, way: "up" | "down") => screen.getByRole<HTMLButtonElement>("button", { name: `Move ${name} ${way} in the coordinator order` });

describe("SettingsMachines", () => {
  it("holds the update checklist", async () => {
    await show(laptopFirst, undefined, [at(ME, 1, { version: "0.9.0", update_ready: "v0.9.1" }), at(OTHER, 1)]);
    const checklist = screen.getByRole("region", { name: "Install the waiting release" });
    expect(within(checklist).getByRole("checkbox", { name: "studio v0.9.1" })).toBeDefined();
  });

  describe("the coordinator order", () => {
    it("lists the listed machines in priority order, each linking to its page", async () => {
      await show();
      expect(names()).toEqual(["laptop", "studio"]);
      expect(within(entry("laptop")).getByRole("link", { name: "laptop" }).getAttribute("href")).toBe(`/settings/machines/${OTHER}`);
    });

    it("leaves a machine the settings do not list out of the order", async () => {
      const given = settings({ machines: [{ id: ME, name: "studio", address: "" }], priority: [ME] });
      await show(given);
      expect(names()).toEqual(["studio"]);
    });

    it("marks the machine that holds the coordinator role, not the first of the order", async () => {
      await show(laptopFirst, [ME]);
      expect(within(entry("studio")).getByText("Coordinator")).toBeDefined();
      expect(within(entry("laptop")).queryByText("Coordinator")).toBeNull();
    });

    it("posts the whole new order when a machine moves", async () => {
      const calls = mockFetch(new Response(null, { status: 204 }));
      await show();

      fireEvent.click(moveButton("laptop", "down"));

      await waitFor(() => expect(calls).toEqual([{ url: "/api/machines/priority", method: "POST", body: { order: [ME, OTHER] } }]));
    });

    // With three machines a wrong neighbour no longer looks right: each move swaps with the next one.
    it.each([
      ["laptop", "up", ["machine-2", "machine-1", "machine-3"]],
      ["desk", "up", ["machine-1", "machine-3", "machine-2"]],
      ["studio", "down", ["machine-2", "machine-1", "machine-3"]],
      ["laptop", "down", ["machine-1", "machine-3", "machine-2"]],
    ] as const)("posts the order with %s moved %s one place among three machines", async (name, way, posted) => {
      const calls = mockFetch(new Response(null, { status: 204 }));
      await show(
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

      fireEvent.click(moveButton(name, way));

      await waitFor(() => expect(calls).toEqual([{ url: "/api/machines/priority", method: "POST", body: { order: posted } }]));
    });

    it("disables the top machine's up and the bottom machine's down", async () => {
      await show();
      expect(moveButton("laptop", "up").disabled).toBe(true);
      expect(moveButton("studio", "down").disabled).toBe(true);
      expect(moveButton("studio", "up").disabled).toBe(false);
    });

    it("keeps both moves disabled after a 204 until the new order arrives", async () => {
      const calls = mockFetch(new Response(null, { status: 204 }));
      const change = await live();
      const moves = () => screen.getAllByRole<HTMLButtonElement>("button", { name: /in the coordinator order/ });

      fireEvent.click(moveButton("laptop", "down"));
      await waitFor(() => expect(calls).toHaveLength(1));
      await act(async () => {});
      expect(moves().every((b) => b.disabled)).toBe(true);

      change(settings({ machines: laptopFirst.machines, priority: [ME, OTHER] }));
      expect(names()).toEqual(["studio", "laptop"]);
      expect(moveButton("studio", "down").disabled).toBe(false);
      expect(calls).toHaveLength(1);

      // Another machine moves it back: nothing is pending, so the moves are on.
      change(laptopFirst);
      expect(names()).toEqual(["laptop", "studio"]);
      expect(moves().filter((b) => b.disabled).map((b) => b.getAttribute("aria-label"))).toEqual(["Move laptop up in the coordinator order", "Move studio down in the coordinator order"]);
    });

    it("shows a refusal's error in an alert", async () => {
      mockFetch(json(422, { error: 'no machine "x" in the machine list' }));
      await show();

      fireEvent.click(moveButton("studio", "up"));

      expect((await screen.findByRole("alert")).textContent).toBe('no machine "x" in the machine list');
      expect(moveButton("studio", "up").disabled).toBe(false);
    });

    it("offers no move when fewer than two machines are listed", async () => {
      await show(settings({ machines: [{ id: ME, name: "studio", address: "" }], priority: [ME] }));
      expect(screen.queryByRole("button", { name: /in the coordinator order/ })).toBeNull();
    });
  });

  describe("adding a machine", () => {
    const added = { code: "K7QX3M9PRT", command: "foundryd join 100.101.102.103 K7QX3M9PRT", expires_in_secs: 600, address: "100.64.0.9" };
    const nameField = () => within(screen.getByRole("dialog")).getByRole<HTMLInputElement>("textbox", { name: "Name" });
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

    it("shows a refusal in the dialog and keeps the text", async () => {
      mockFetch(json(409, { error: 'a machine named "laptop" is already in the fleet' }));
      await show();

      add("Laptop");

      expect((await screen.findByRole("alert")).textContent).toBe('a machine named "laptop" is already in the fleet');
      expect(nameField().value).toBe("Laptop");
    });

    it("says the machine joined once it shows in the settings", async () => {
      mockFetch(json(201, added));
      const change = await live();

      add("Trilogy-Mac");
      await screen.findByText(added.command);
      expect(screen.queryByText(/joined/)).toBeNull();
      change(settings({ machines: [...laptopFirst.machines, { id: "m-3", name: "trilogy-mac", address: "100.64.0.3" }], priority: laptopFirst.priority }));

      expect((await screen.findByRole("status")).textContent).toBe("Trilogy-Mac joined");
    });

    it("says the machine joined when it lists itself under its own name at the address the typed name resolved to", async () => {
      mockFetch(json(201, { ...added, address: "100.64.0.3" }));
      const change = await live();

      add("trilogy-mac.tail459612.ts.net");
      await screen.findByText(added.command);
      change(settings({ machines: [...laptopFirst.machines, { id: "m-3", name: "trilogy-mac", address: "100.64.0.3" }], priority: laptopFirst.priority }));

      expect((await screen.findByRole("status")).textContent).toBe("trilogy-mac.tail459612.ts.net joined");
    });
  });
});
