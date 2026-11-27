import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { AnyRecord, MachineSample } from "./api";
import { fleet } from "./fleet";
import { ME, OTHER, sample, settings } from "./test-fixtures";
import { UpdateBadge } from "./UpdateBadge";

/** Each install call the badge made, answered when the test says. */
const sent = vi.hoisted(() => ({
  calls: [] as { machine: string; tag: string; answer: (a: { status: number; json: Record<string, unknown> }) => void; drop: (e: Error) => void }[],
}));

vi.mock("./api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./api")>()),
  installUpdate: vi.fn(
    (machine: string, tag: string) =>
      new Promise((answer, drop) => {
        sent.calls.push({ machine, tag, answer, drop });
      }),
  ),
}));

const THIRD = "machine-3";
const GONE = "machine-4";

const three = settings({
  machines: [
    { id: ME, name: "studio", address: "" },
    { id: OTHER, name: "laptop", address: "" },
    { id: THIRD, name: "mini", address: "" },
  ],
});

/** A sample of `owner` with the update fields given. */
function at(owner: string, version: number, update: Partial<Pick<MachineSample, "version" | "update_ready" | "update_refused">>): AnyRecord {
  const r = sample(owner, version) as AnyRecord & { kind: "machine_sample" };
  return { ...r, body: { ...r.body, ...update } };
}

const machinesOf = (records: AnyRecord[]) => fleet(records, three);

/** Shows the badge over `records`, served by `here`; `null` for a page that does not know its machine yet. */
function show(records: AnyRecord[], served: string | null = ME) {
  const here = served ?? undefined;
  const view = render(<UpdateBadge machines={machinesOf(records)} here={here} />);
  return { ...view, update: (next: AnyRecord[]) => view.rerender(<UpdateBadge machines={machinesOf(next)} here={here} />) };
}

const badge = () => screen.getByRole("button", { name: /waiting/i });
const dialog = () => screen.getByRole("dialog");
const row = (name: string) => within(dialog()).getByRole("listitem", { name });
const settle = () => act(async () => {});

beforeEach(() => {
  sent.calls = [];
});

afterEach(() => vi.clearAllMocks());

describe("UpdateBadge", () => {
  it("shows nothing while no machine has a release waiting", () => {
    const { container } = show([at(ME, 1, { version: "0.9.0" }), at(OTHER, 1, { version: "0.9.0" })]);
    expect(container.innerHTML).toBe("");
  });

  it("names each waiting tag and how many machines wait on it", () => {
    // The lower tag's machine comes first, so the order is the badge's own: newest first.
    show([at(ME, 1, { version: "0.8.0", update_ready: "v0.9.0" }), at(OTHER, 1, { version: "0.9.0", update_ready: "v0.9.1" }), at(THIRD, 1, { version: "0.9.0", update_ready: "v0.9.1" })]);
    expect(badge().textContent).toContain("v0.9.1 on 2 machines, v0.9.0 on 1");
  });

  it("leaves out a removed machine", () => {
    show([at(ME, 1, { version: "0.9.0", update_ready: "v0.9.1" }), at(GONE, 1, { version: "0.9.0", update_ready: "v0.9.1" })]);
    expect(badge().textContent).toContain("v0.9.1 on 1 machine");
    expect(badge().textContent).not.toContain("2 machines");
  });

  it("opens a checklist of the waiting machines, every one ticked", () => {
    show([at(ME, 1, { version: "0.9.0", update_ready: "v0.9.1" }), at(OTHER, 1, { version: "0.9.0", update_ready: "v0.9.1" }), at(THIRD, 1, { version: "0.9.1" })]);
    fireEvent.click(badge());
    const boxes = within(dialog()).getAllByRole("checkbox");
    expect(boxes.map((b) => b.getAttribute("aria-label"))).toEqual(["studio v0.9.1", "laptop v0.9.1"]);
    expect(boxes.every((b) => (b as HTMLInputElement).checked)).toBe(true);
  });

  it("sends one call per ticked machine with the tag shown at the click, even when a record changes it before the call goes", async () => {
    const before = [at(ME, 1, { version: "0.9.0", update_ready: "v0.9.1" }), at(OTHER, 1, { version: "0.9.0", update_ready: "v0.9.1" }), at(THIRD, 1, { version: "0.9.0", update_ready: "v0.9.1" })];
    const { update } = show(before);
    fireEvent.click(badge());
    fireEvent.click(within(dialog()).getByRole("checkbox", { name: "mini v0.9.1" }));
    fireEvent.click(within(dialog()).getByRole("button", { name: "Install" }));
    // The serving machine's ready tag moves on while the laptop's call is still out.
    update([at(ME, 2, { version: "0.9.0", update_ready: "v0.9.2" }), before[1], before[2]]);
    expect(sent.calls.map((c) => [c.machine, c.tag])).toEqual([[OTHER, "v0.9.1"]]);
    act(() => sent.calls[0].answer({ status: 202, json: {} }));
    await settle();
    expect(sent.calls.map((c) => [c.machine, c.tag])).toEqual([
      [OTHER, "v0.9.1"],
      [ME, "v0.9.1"],
    ]);
  });

  it("sends the serving machine's call only once every other call has answered, failed or dropped", async () => {
    show([
      at(ME, 1, { version: "0.9.0", update_ready: "v0.9.1" }),
      at(OTHER, 1, { version: "0.9.0", update_ready: "v0.9.1" }),
      at(THIRD, 1, { version: "0.9.0", update_ready: "v0.9.1" }),
    ]);
    fireEvent.click(badge());
    fireEvent.click(within(dialog()).getByRole("button", { name: "Install" }));
    expect(sent.calls.map((c) => c.machine)).toEqual([OTHER, THIRD]);
    expect(within(row("studio")).getByText(/waits for the others/i)).toBeDefined();
    act(() => sent.calls[0].answer({ status: 500, json: { error: "boom" } }));
    await settle();
    expect(sent.calls).toHaveLength(2);
    act(() => sent.calls[1].drop(new TypeError("Failed to fetch")));
    await settle();
    expect(sent.calls.map((c) => c.machine)).toEqual([OTHER, THIRD, ME]);
  });

  it("waits to install until it knows which machine serves the page", () => {
    show([at(ME, 1, { version: "0.9.0", update_ready: "v0.9.1" })], null);
    fireEvent.click(badge());
    expect((within(dialog()).getByRole("button", { name: "Install" }) as HTMLButtonElement).disabled).toBe(true);
  });

  describe("each row says, first match winning", () => {
    /** Installs v0.9.1 on the laptop alone, answered with `answer`, and gives back the update function. */
    async function installed(answer: ((c: (typeof sent.calls)[number]) => void) | undefined) {
      const shown = show([at(ME, 1, { version: "0.9.1" }), at(OTHER, 1, { version: "0.9.0", update_ready: "v0.9.1" })]);
      fireEvent.click(badge());
      fireEvent.click(within(dialog()).getByRole("button", { name: "Install" }));
      if (answer) act(() => answer(sent.calls[0]));
      await settle();
      return shown.update;
    }
    const says = (text: RegExp) => expect(within(row("laptop")).getByText(text)).toBeDefined();

    it("1. Refused, when its update_refused is the tag, whatever the call answered", async () => {
      const update = await installed((c) => c.answer({ status: 202, json: {} }));
      update([at(ME, 1, { version: "0.9.1" }), at(OTHER, 2, { version: "0.9.0", update_refused: "v0.9.1" })]);
      says(/^Refused/);
    });

    it("1. Refused, even when its version matches the tag", async () => {
      const update = await installed((c) => c.answer({ status: 202, json: {} }));
      update([at(ME, 1, { version: "0.9.1" }), at(OTHER, 2, { version: "0.9.1", update_refused: "v0.9.1" })]);
      says(/^Refused/);
    });

    it("2. Running the tag, when its version matches the tag, turning into Refused if the trial fails", async () => {
      const update = await installed((c) => c.answer({ status: 202, json: {} }));
      says(/^Restarting/);
      update([at(ME, 1, { version: "0.9.1" }), at(OTHER, 2, { version: "0.9.1" })]);
      says(/^Running v0\.9\.1/);
      update([at(ME, 1, { version: "0.9.1" }), at(OTHER, 3, { version: "0.9.0", update_refused: "v0.9.1" })]);
      says(/^Refused/);
    });

    it("compares the version with the tag as three numbers", async () => {
      const update = await installed((c) => c.answer({ status: 504, json: {} }));
      update([at(ME, 1, { version: "0.9.1" }), at(OTHER, 2, { version: "0.9.10" })]);
      says(/^Sent, outcome unknown/);
      update([at(ME, 1, { version: "0.9.1" }), at(OTHER, 3, { version: "0.09.01" })]);
      says(/^Running v0\.9\.1/);
    });

    it("3. Restarting, when the call answered 202", async () => {
      await installed((c) => c.answer({ status: 202, json: {} }));
      says(/^Restarting/);
    });

    it("4. Sent, outcome unknown, when the call answered 504", async () => {
      await installed((c) => c.answer({ status: 504, json: { error: "the link went down" } }));
      says(/^Sent, outcome unknown/);
      expect(sent.calls).toHaveLength(1);
    });

    it("4. Sent, outcome unknown, when the request failed after it was sent, and nothing is sent again", async () => {
      await installed((c) => c.drop(new TypeError("Failed to fetch")));
      says(/^Sent, outcome unknown/);
      expect(sent.calls).toHaveLength(1);
    });

    it("5. Not sent, with the error text, when the call answered 503 or 404", async () => {
      await installed((c) => c.answer({ status: 503, json: { error: "laptop is offline" } }));
      says(/^Not sent: laptop is offline/);
    });

    it("5. Not sent, for a 404", async () => {
      await installed((c) => c.answer({ status: 404, json: { error: "no machine laptop" } }));
      says(/^Not sent: no machine laptop/);
    });

    it("6. Already changed, with the error text and the tag now ready, when the call answered 409", async () => {
      await installed((c) => c.answer({ status: 409, json: { error: "v0.9.1 is not ready", ready: "v0.9.2" } }));
      says(/^Already changed: v0\.9\.1 is not ready/);
      says(/v0\.9\.2 is ready now/);
    });

    it("6. Already changed, with no tag when none is ready", async () => {
      await installed((c) => c.answer({ status: 409, json: { error: "nothing is ready", ready: null } }));
      says(/^Already changed: nothing is ready/);
      expect(within(row("laptop")).queryByText(/is ready now/)).toBeNull();
    });

    it("7. Failed, with the error text, on any other answer", async () => {
      await installed((c) => c.answer({ status: 500, json: { error: "could not point current" } }));
      says(/^Failed: could not point current/);
    });
  });

  it("offers the checklist again on reopening, with every waiting machine ticked and Install back", async () => {
    show([at(ME, 1, { version: "0.9.1" }), at(OTHER, 1, { version: "0.9.0", update_ready: "v0.9.1" })]);
    fireEvent.click(badge());
    fireEvent.click(within(dialog()).getByRole("button", { name: "Install" }));
    act(() => sent.calls[0].answer({ status: 503, json: { error: "laptop is offline" } }));
    await settle();
    fireEvent.click(within(dialog()).getByRole("button", { name: "Close" }));
    fireEvent.click(badge());
    expect((within(dialog()).getByRole("checkbox", { name: "laptop v0.9.1" }) as HTMLInputElement).checked).toBe(true);
    expect(within(dialog()).queryByText(/Not sent/)).toBeNull();
    expect(within(dialog()).getByRole("button", { name: "Install" })).toBeDefined();
  });

  it("offers no Install while an earlier checklist's calls are still out, and shows none of their answers", async () => {
    show([at(ME, 1, { version: "0.9.1" }), at(OTHER, 1, { version: "0.9.0", update_ready: "v0.9.1" })]);
    fireEvent.click(badge());
    fireEvent.click(within(dialog()).getByRole("button", { name: "Install" }));
    fireEvent.click(within(dialog()).getByRole("button", { name: "Close" }));
    fireEvent.click(badge());
    expect((within(dialog()).getByRole("button", { name: "Install" }) as HTMLButtonElement).disabled).toBe(true);
    expect(within(dialog()).getByText(/still out/i)).toBeDefined();
    act(() => sent.calls[0].answer({ status: 500, json: { error: "from the first checklist" } }));
    await settle();
    expect(within(dialog()).queryByText(/from the first checklist/)).toBeNull();
    expect((within(dialog()).getByRole("button", { name: "Install" }) as HTMLButtonElement).disabled).toBe(false);
    expect(sent.calls).toHaveLength(1);
  });

  it("sends the serving machine's install when its turn comes, even after the checklist that took it is closed", async () => {
    show([at(ME, 1, { version: "0.9.0", update_ready: "v0.9.1" }), at(OTHER, 1, { version: "0.9.0", update_ready: "v0.9.1" })]);
    fireEvent.click(badge());
    fireEvent.click(within(dialog()).getByRole("button", { name: "Install" }));
    fireEvent.click(within(dialog()).getByRole("button", { name: "Close" }));
    act(() => sent.calls[0].answer({ status: 202, json: {} }));
    await settle();
    expect(sent.calls.map((c) => [c.machine, c.tag])).toEqual([
      [OTHER, "v0.9.1"],
      [ME, "v0.9.1"],
    ]);
  });

  it("offers no Install once the serving machine has answered 202, since it is restarting", async () => {
    show([at(ME, 1, { version: "0.9.0", update_ready: "v0.9.1" }), at(OTHER, 1, { version: "0.9.0", update_ready: "v0.9.1" })]);
    fireEvent.click(badge());
    fireEvent.click(within(dialog()).getByRole("checkbox", { name: "laptop v0.9.1" }));
    fireEvent.click(within(dialog()).getByRole("button", { name: "Install" }));
    await settle();
    act(() => sent.calls[0].answer({ status: 202, json: {} }));
    await settle();
    fireEvent.click(within(dialog()).getByRole("button", { name: "Close" }));
    fireEvent.click(badge());
    expect((within(dialog()).getByRole("button", { name: "Install" }) as HTMLButtonElement).disabled).toBe(true);
    expect(within(dialog()).getByText(/this machine is restarting/i)).toBeDefined();
  });

  it("offers no Install while the serving machine's own call is out", async () => {
    show([at(ME, 1, { version: "0.9.0", update_ready: "v0.9.1" }), at(OTHER, 1, { version: "0.9.0", update_ready: "v0.9.1" })]);
    fireEvent.click(badge());
    fireEvent.click(within(dialog()).getByRole("button", { name: "Install" }));
    act(() => sent.calls[0].answer({ status: 202, json: {} }));
    await settle();
    expect(sent.calls.map((c) => c.machine)).toEqual([OTHER, ME]);
    fireEvent.click(within(dialog()).getByRole("button", { name: "Close" }));
    fireEvent.click(badge());
    expect((within(dialog()).getByRole("button", { name: "Install" }) as HTMLButtonElement).disabled).toBe(true);
    expect(within(dialog()).getByText(/still out/i)).toBeDefined();
  });

  it("holds nothing back when the serving machine's call answers anything but 202", async () => {
    show([at(ME, 1, { version: "0.9.0", update_ready: "v0.9.1" }), at(OTHER, 1, { version: "0.9.0", update_ready: "v0.9.1" })]);
    fireEvent.click(badge());
    fireEvent.click(within(dialog()).getByRole("checkbox", { name: "laptop v0.9.1" }));
    fireEvent.click(within(dialog()).getByRole("button", { name: "Install" }));
    await settle();
    act(() => sent.calls[0].answer({ status: 500, json: { error: "could not point current" } }));
    await settle();
    fireEvent.click(within(dialog()).getByRole("button", { name: "Close" }));
    fireEvent.click(badge());
    expect((within(dialog()).getByRole("button", { name: "Install" }) as HTMLButtonElement).disabled).toBe(false);
    expect(within(dialog()).queryByText(/restarting/i)).toBeNull();
  });

  for (const [back, sampled] of [
    ["runs the tag", { version: "0.9.1" }],
    ["refused the tag", { version: "0.9.0", update_refused: "v0.9.1" }],
  ] as const) {
    it(`offers Install again once the serving machine's sample says it is back and ${back}, with no reload`, async () => {
      const { update } = show([at(ME, 1, { version: "0.9.0", update_ready: "v0.9.1" }), at(OTHER, 1, { version: "0.9.0", update_ready: "v0.9.1" })]);
      fireEvent.click(badge());
      fireEvent.click(within(dialog()).getByRole("checkbox", { name: "laptop v0.9.1" }));
      fireEvent.click(within(dialog()).getByRole("button", { name: "Install" }));
      await settle();
      act(() => sent.calls[0].answer({ status: 202, json: {} }));
      await settle();
      fireEvent.click(within(dialog()).getByRole("button", { name: "Close" }));
      update([at(ME, 2, sampled), at(OTHER, 1, { version: "0.9.0", update_ready: "v0.9.1" })]);
      fireEvent.click(badge());
      expect((within(dialog()).getByRole("button", { name: "Install" }) as HTMLButtonElement).disabled).toBe(false);
      expect(within(dialog()).queryByText(/restarting/i)).toBeNull();
      // A later release reaches it from elsewhere: the hold, lifted once, stays lifted.
      update([at(ME, 3, { version: "0.9.2" }), at(OTHER, 1, { version: "0.9.0", update_ready: "v0.9.1" })]);
      expect((within(dialog()).getByRole("button", { name: "Install" }) as HTMLButtonElement).disabled).toBe(false);
      expect(within(dialog()).queryByText(/restarting/i)).toBeNull();
    });
  }

  it("keeps a row after its machine's update_ready clears, until the checklist is closed", async () => {
    const { update } = show([at(ME, 1, { version: "0.9.1" }), at(OTHER, 1, { version: "0.9.0", update_ready: "v0.9.1" })]);
    fireEvent.click(badge());
    fireEvent.click(within(dialog()).getByRole("button", { name: "Install" }));
    act(() => sent.calls[0].answer({ status: 202, json: {} }));
    await settle();
    update([at(ME, 1, { version: "0.9.1" }), at(OTHER, 2, { version: "0.9.1" })]);
    expect(within(row("laptop")).getByText(/^Running v0\.9\.1/)).toBeDefined();
    fireEvent.click(within(dialog()).getByRole("button", { name: "Close" }));
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.queryByRole("button", { name: /waiting/i })).toBeNull();
  });
});
