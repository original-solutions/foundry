import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AnyRecord, MachineSample, SharedSettings } from "./api";
import { emptyState, fleet } from "./fleet";
import { SettingsMachine } from "./SettingsMachine";
import { forgetInstalls } from "./UpdateChecklist";
import { ME, OTHER, sample, settings } from "./test-fixtures";
import { json, mockFetch, pageProps, renderApp } from "./test-utils";

afterEach(() => {
  vi.unstubAllGlobals();
  forgetInstalls();
});

const limited = settings({
  machines: [
    { id: ME, name: "studio", address: "" },
    { id: OTHER, name: "laptop", address: "" },
  ],
  priority: [ME, OTHER],
  limits: { [OTHER]: { max_sessions: 4, max_load_per_core: 1.5, min_free_disk: 20_000_000_000 } },
});

/** A sample of `owner` with the update fields given. */
function at(owner: string, version: number, update: Partial<Pick<MachineSample, "version" | "update_ready" | "update_refused">> = {}): AnyRecord {
  const r = sample(owner, version) as AnyRecord & { kind: "machine_sample" };
  return { ...r, body: { ...r.body, ...update } };
}

/** Shows machine `id`'s page, served by `here`, with `online` the machines the daemon hears. */
function show(id: string, extra: { records?: AnyRecord[]; given?: SharedSettings; online?: string[]; here?: string } = {}) {
  const given = extra.given ?? limited;
  const machines = fleet(extra.records ?? [at(ME, 1), at(OTHER, 1)], given, {}, extra.online ?? [ME, OTHER]);
  const state = { ...emptyState, synced: true, settings: given };
  return renderApp(<SettingsMachine {...pageProps({ state, machines, here: extra.here ?? ME })} machineId={id} />);
}

/** The input a label names; the review bar's entry for a change carries the same name. */
const field = (label: string) => screen.getAllByLabelText<HTMLInputElement>(label).find((el) => el.tagName === "INPUT")!;
const type = (label: string, value: string) => fireEvent.change(field(label), { target: { value } });
const bar = () => screen.queryByRole("region", { name: "Unsaved changes" });
const row = (label: string) => field(label).closest("[data-row]") as HTMLElement;

describe("SettingsMachine", () => {
  it("shows its sections in order: the update, the shared settings, then Remove", async () => {
    await show(OTHER, { records: [at(ME, 1), at(OTHER, 1, { version: "0.9.0", update_ready: "v0.9.1" })] });
    const headings = screen.getAllByRole("heading", { level: 3 }).map((h) => h.textContent);
    expect(headings).toEqual(["Update", "In the shared settings", "Remove from the fleet"]);
  });

  it("fills the name and limits from the settings, the disk in gigabytes, with no review bar until an edit", async () => {
    await show(OTHER);
    expect(field("Name").value).toBe("laptop");
    expect(field("Max sessions").value).toBe("4");
    expect(field("Max load per core").value).toBe("1.5");
    expect(field("Min free disk (GB)").value).toBe("20");
    expect(bar()).toBeNull();
  });

  it("joins name and limits in the review bar and saves them with one request each", async () => {
    const calls = mockFetch(new Response(null, { status: 204 }), new Response(null, { status: 204 }));
    await show(OTHER);

    type("Name", "  desk ");
    type("Max sessions", "6");
    type("Min free disk (GB)", "4.35");
    expect(bar()?.textContent).toContain("3 changes");
    expect(bar()?.textContent).toContain("Name laptop → desk");
    expect(bar()?.textContent).toContain("Max sessions 4 → 6");
    fireEvent.click(screen.getByRole("button", { name: "Save 3 changes" }));

    await waitFor(() => expect(bar()).toBeNull());
    expect(calls).toEqual([
      { url: `/api/machines/${OTHER}/rename`, method: "POST", body: { name: "desk" } },
      { url: `/api/machines/${OTHER}/limits`, method: "POST", body: { max_sessions: 6, max_load_per_core: 1.5, min_free_disk_gb: 4.35 } },
    ]);
    // What was saved stays shown until the settings that carry it arrive.
    expect(field("Name").value).toBe("desk");
  });

  it("sends the stored disk limit exactly when its field is left alone", async () => {
    const calls = mockFetch(new Response(null, { status: 204 }));
    await show(OTHER, { given: { ...limited, limits: { [OTHER]: { max_sessions: 4, max_load_per_core: 1.5, min_free_disk: 4_355_500_000 } } } });
    expect(field("Min free disk (GB)").value).toBe("4.3555");
    type("Max sessions", "5");
    fireEvent.click(screen.getByRole("button", { name: "Save 1 change" }));
    await waitFor(() => expect(bar()).toBeNull());
    expect(calls[0].body).toEqual({ max_sessions: 5, max_load_per_core: 1.5, min_free_disk_gb: 4.3555 });
  });

  it("shows Clear limits as one change to none, and sends it to the clear route", async () => {
    const calls = mockFetch(new Response(null, { status: 204 }));
    await show(OTHER);

    fireEvent.click(screen.getByRole("button", { name: "Clear limits" }));
    expect(bar()?.textContent).toContain("1 change");
    expect(bar()?.textContent).toContain("Limits At most 4 sessions · load 1.5 per core · 20 GB disk free → none");
    fireEvent.click(screen.getByRole("button", { name: "Save 1 change" }));

    await waitFor(() => expect(bar()).toBeNull());
    expect(calls).toEqual([{ url: `/api/machines/${OTHER}/limits/clear`, method: "POST", body: undefined }]);
  });

  it("offers no Clear limits for a machine with none", async () => {
    await show(ME);
    expect(screen.getByRole<HTMLButtonElement>("button", { name: "Clear limits" }).disabled).toBe(true);
  });

  it("Discard puts the saved values back", async () => {
    await show(OTHER);
    type("Name", "desk");
    fireEvent.click(screen.getByRole("button", { name: "Clear limits" }));
    fireEvent.click(screen.getByRole("button", { name: "Discard" }));
    expect(bar()).toBeNull();
    expect(field("Name").value).toBe("laptop");
    expect(field("Max sessions").value).toBe("4");
  });

  it("keeps a refused change in the bar with the daemon's reason, and lets the saved one leave", async () => {
    const calls = mockFetch(json(409, { error: 'another machine is named "studio"' }), new Response(null, { status: 204 }));
    await show(OTHER);

    type("Name", "studio");
    type("Max sessions", "6");
    fireEvent.click(screen.getByRole("button", { name: "Save 2 changes" }));

    await waitFor(() => expect(row("Name").textContent).toContain('another machine is named "studio"'));
    expect(bar()?.textContent).toContain("1 change");
    expect(bar()?.textContent).toContain("Name laptop → studio");
    expect(bar()?.textContent).not.toContain("Max sessions");
    expect(calls).toHaveLength(2);
  });

  it("keeps a refused limits change in the bar with the daemon's reason beside its field", async () => {
    const calls = mockFetch(new Response(null, { status: 204 }), json(503, { error: "the coordinator is not reachable" }));
    await show(OTHER);

    type("Name", "desk");
    type("Max sessions", "6");
    fireEvent.click(screen.getByRole("button", { name: "Save 2 changes" }));

    await waitFor(() => expect(row("Max sessions").textContent).toContain("the coordinator is not reachable"));
    expect(bar()?.textContent).toContain("Max sessions 4 → 6");
    expect(bar()?.textContent).not.toContain("Name laptop");
    expect(calls).toHaveLength(2);
  });

  it.each([
    ["Max sessions", "0", "the maximum sessions is a whole number from 1 to 4294967295"],
    ["Max sessions", "4294967296", "the maximum sessions is a whole number from 1 to 4294967295"],
    ["Max load per core", "", "the maximum load per core is a number above 0"],
    ["Min free disk (GB)", "", "the minimum free disk is a number of gigabytes, 0 or more"],
    ["Max sessions", "2.5", "the maximum sessions is a whole number from 1 to 4294967295"],
    ["Max sessions", "", "the maximum sessions is a whole number from 1 to 4294967295"],
    ["Max load per core", "0", "the maximum load per core is a number above 0"],
    ["Min free disk (GB)", "-1", "the minimum free disk is a number of gigabytes, 0 or more"],
  ])("checks %s %j against today's range before sending, and says why beside it", async (label, value, message) => {
    const calls = mockFetch(new Response(null, { status: 204 }));
    await show(OTHER);

    type("Name", "desk");
    type(label, value);
    fireEvent.click(screen.getByRole("button", { name: "Save 2 changes" }));

    await waitFor(() => expect(row(label).textContent).toContain(message));
    // The limits are not sent; the name, which passes, is.
    expect(calls).toEqual([{ url: `/api/machines/${OTHER}/rename`, method: "POST", body: { name: "desk" } }]);
  });

  it("counts a name typed with spaces around the one it has as no change", async () => {
    await show(OTHER);
    type("Name", " laptop ");
    expect(bar()).toBeNull();
  });

  it("locks the name, limits and Clear limits, and offers no Remove, for a machine the machine list does not hold", async () => {
    const given = settings({ machines: [{ id: ME, name: "studio", address: "" }], priority: [ME] });
    await show(OTHER, { given });
    expect(screen.getByText(/does not hold this machine yet/)).toBeDefined();
    expect(field("Name").disabled).toBe(true);
    expect(field("Max sessions").disabled).toBe(true);
    expect(screen.getByRole<HTMLButtonElement>("button", { name: "Clear limits" }).disabled).toBe(true);
    expect(screen.queryByRole("button", { name: "Remove from fleet" })).toBeNull();
  });

  it("keeps the shared section editable, and saving, while the machine is offline", async () => {
    const calls = mockFetch(new Response(null, { status: 204 }));
    await show(OTHER, { online: [ME] });

    expect(field("Name").disabled).toBe(false);
    expect(field("Max sessions").disabled).toBe(false);
    type("Name", "desk");
    fireEvent.click(screen.getByRole("button", { name: "Save 1 change" }));

    await waitFor(() => expect(bar()).toBeNull());
    expect(calls).toEqual([{ url: `/api/machines/${OTHER}/rename`, method: "POST", body: { name: "desk" } }]);
  });

  /** Shows laptop's page over settings the test can change, as the events socket would. */
  async function live() {
    let change: (next: SharedSettings) => void = () => {};
    function Live() {
      const [given, setGiven] = useState(limited);
      change = setGiven;
      const machines = fleet([at(ME, 1), at(OTHER, 1)], given, {}, [ME, OTHER]);
      return <SettingsMachine {...pageProps({ state: { ...emptyState, synced: true, settings: given }, machines, here: ME })} machineId={OTHER} />;
    }
    await renderApp(<Live />);
    return (next: Partial<SharedSettings>) => act(() => change({ ...limited, seq: 2, ...next }));
  }
  const renamed = (name: string) => ({ machines: [limited.machines[0], { id: OTHER, name, address: "" }] });

  it("lets untouched fields follow the settings as they change", async () => {
    const change = await live();
    type("Name", "desk");
    change({ limits: { [OTHER]: { max_sessions: 9, max_load_per_core: 1.5, min_free_disk: 20_000_000_000 } } });
    expect(field("Max sessions").value).toBe("9");
    expect(field("Name").value).toBe("desk");
    expect(bar()?.textContent).toContain("1 change");
  });

  it("marks a field being edited that changed elsewhere, and asks for a fresh look before Save", async () => {
    const calls = mockFetch(new Response(null, { status: 204 }));
    const change = await live();
    type("Name", "desk");
    change(renamed("den"));

    expect(bar()?.textContent).toContain("Name den → desk");
    expect(bar()?.textContent).toContain("changed elsewhere");
    fireEvent.click(screen.getByRole("button", { name: "Look again" }));
    expect(calls).toEqual([]);
    fireEvent.click(screen.getByRole("button", { name: "Save 1 change" }));

    await waitFor(() => expect(bar()).toBeNull());
    expect(calls).toEqual([{ url: `/api/machines/${OTHER}/rename`, method: "POST", body: { name: "desk" } }]);
  });

  it("shows a saved name until the settings carry it, then follows them again", async () => {
    mockFetch(new Response(null, { status: 204 }));
    const change = await live();
    type("Name", "desk");
    fireEvent.click(screen.getByRole("button", { name: "Save 1 change" }));
    await waitFor(() => expect(bar()).toBeNull());
    expect(field("Name").value).toBe("desk");
    change(renamed("desk"));
    expect(field("Name").value).toBe("desk");
    change({ ...renamed("den"), seq: 3 });
    expect(field("Name").value).toBe("den");
    expect(bar()).toBeNull();
  });

  it("keeps a decimal as typed over a saved whole number, and counts it a change only once it differs", async () => {
    const calls = mockFetch(new Response(null, { status: 204 }));
    await show(ME, { given: { ...limited, limits: { [ME]: { max_sessions: 4, max_load_per_core: 1, min_free_disk: 20_000_000_000 } } } });
    type("Max load per core", "1.0");
    expect(field("Max load per core").value).toBe("1.0");
    expect(bar()).toBeNull();
    type("Max load per core", "1.05");
    expect(field("Max load per core").value).toBe("1.05");
    expect(bar()?.textContent).toContain("Max load per core 1 → 1.05");
    fireEvent.click(screen.getByRole("button", { name: "Save 1 change" }));
    await waitFor(() => expect(bar()).toBeNull());
    expect(calls).toEqual([{ url: `/api/machines/${ME}/limits`, method: "POST", body: { max_sessions: 4, max_load_per_core: 1.05, min_free_disk_gb: 20 } }]);
  });

  it("shows a refused Clear limits on the limit rows when the rename went through", async () => {
    mockFetch(new Response(null, { status: 204 }), json(503, { error: "the coordinator is not reachable" }));
    await show(OTHER);
    type("Name", "desk");
    fireEvent.click(screen.getByRole("button", { name: "Clear limits" }));
    fireEvent.click(screen.getByRole("button", { name: "Save 2 changes" }));
    await waitFor(() => expect(row("Max sessions").textContent).toContain("the coordinator is not reachable"));
    expect(bar()?.textContent).toContain("1 change");
    type("Max sessions", "6");
    expect(row("Max sessions").textContent).not.toContain("the coordinator is not reachable");
    expect(row("Max load per core").textContent).not.toContain("the coordinator is not reachable");
  });

  it("saves a rename and Clear limits together with one request each", async () => {
    const calls = mockFetch(new Response(null, { status: 204 }), new Response(null, { status: 204 }));
    await show(OTHER);
    type("Name", "desk");
    fireEvent.click(screen.getByRole("button", { name: "Clear limits" }));
    expect(bar()?.textContent).toContain("2 changes");
    expect(bar()?.textContent).toContain("Name laptop → desk");
    expect(bar()?.textContent).toMatch(/Limits .* → none/);
    fireEvent.click(screen.getByRole("button", { name: "Save 2 changes" }));
    await waitFor(() => expect(bar()).toBeNull());
    expect(calls).toEqual([
      { url: `/api/machines/${OTHER}/rename`, method: "POST", body: { name: "desk" } },
      { url: `/api/machines/${OTHER}/limits/clear`, method: "POST", body: undefined },
    ]);
  });

  it("asks for a fresh look, sending nothing, when the limits changed elsewhere under a Clear limits", async () => {
    const calls = mockFetch(new Response(null, { status: 204 }));
    const change = await live();
    fireEvent.click(screen.getByRole("button", { name: "Clear limits" }));
    change({ limits: { [OTHER]: { max_sessions: 9, max_load_per_core: 1.5, min_free_disk: 20_000_000_000 } } });
    expect(bar()?.textContent).toContain("changed elsewhere");
    fireEvent.click(screen.getByRole("button", { name: "Look again" }));
    expect(calls).toEqual([]);
  });

  it("keeps typing made while a save is out, and still counts it one change", async () => {
    const out: { url: string; answer: (r: Response) => void }[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => new Promise<Response>((answer) => out.push({ url, answer }))),
    );
    await show(OTHER);
    type("Name", "desk");
    fireEvent.click(screen.getByRole("button", { name: "Save 1 change" }));
    await waitFor(() => expect(out).toHaveLength(1));
    type("Name", "desk2");
    await act(async () => out[0].answer(new Response(null, { status: 204 })));
    expect(field("Name").value).toBe("desk2");
    expect(bar()?.textContent).toContain("1 change");
    expect(bar()?.textContent).not.toContain("changed elsewhere");
    expect(screen.getByRole("button", { name: "Save 1 change" })).toBeDefined();
  });

  /** Stubs `fetch` so each call waits until the test answers it. */
  function held() {
    const out: { url: string; answer: (r: Response) => void }[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => new Promise<Response>((answer) => out.push({ url, answer }))),
    );
    return out;
  }

  it("does not mark a field typed into while a save was out as changed elsewhere, whatever was sent", async () => {
    const out = held();
    await show(OTHER);
    type("Name", "desk ");
    fireEvent.click(screen.getByRole("button", { name: "Save 1 change" }));
    await waitFor(() => expect(out).toHaveLength(1));
    type("Name", "desk2");
    await act(async () => out[0].answer(new Response(null, { status: 204 })));
    expect(bar()?.textContent).not.toContain("changed elsewhere");
    expect(screen.getByRole("button", { name: "Save 1 change" })).toBeDefined();
  });

  it("does not mark a limit typed into while another limit's save was out as changed elsewhere", async () => {
    const out = held();
    await show(OTHER);
    type("Max sessions", "5");
    fireEvent.click(screen.getByRole("button", { name: "Save 1 change" }));
    await waitFor(() => expect(out).toHaveLength(1));
    type("Max load per core", "2");
    await act(async () => out[0].answer(new Response(null, { status: 204 })));
    expect(row("Max load per core").textContent).not.toContain("changed elsewhere");
    expect(bar()?.textContent).not.toContain("changed elsewhere");
    expect(screen.getByRole("button", { name: "Save 1 change" })).toBeDefined();
  });

  it("keeps a field typed back to its saved value while its save was out", async () => {
    const out = held();
    await show(OTHER);
    type("Name", "desk");
    fireEvent.click(screen.getByRole("button", { name: "Save 1 change" }));
    await waitFor(() => expect(out).toHaveLength(1));
    type("Name", "laptop");
    await act(async () => out[0].answer(new Response(null, { status: 204 })));
    expect(field("Name").value).toBe("laptop");
    expect(bar()?.textContent).toContain("1 change");
    expect(bar()?.textContent).toContain("Name desk → laptop");
  });

  describe("the update", () => {
    const ready = (id: string) => [at(ME, 1, id === ME ? { version: "0.9.0", update_ready: "v0.9.1" } : {}), at(OTHER, 1, id === OTHER ? { version: "0.9.0", update_ready: "v0.9.1" } : {})];

    /** Stubs `fetch` so each install waits until the test answers it. */
    function pending() {
      const out: { url: string; answer: (r: Response) => void; drop: (e: Error) => void }[] = [];
      vi.stubGlobal(
        "fetch",
        vi.fn((url: string) => new Promise<Response>((answer, drop) => out.push({ url, answer, drop }))),
      );
      return out;
    }

    /** Shows machine pages over a fleet where both machines have v0.9.1 ready, and gives back how to open another machine's page. */
    async function both(start: string) {
      let open: (id: string) => void = () => {};
      const machines = fleet([at(ME, 1, { version: "0.9.0", update_ready: "v0.9.1" }), at(OTHER, 1, { version: "0.9.0", update_ready: "v0.9.1" })], limited, {}, [ME, OTHER]);
      function Live() {
        const [id, setId] = useState(start);
        open = setId;
        return <SettingsMachine {...pageProps({ state: { ...emptyState, synced: true, settings: limited }, machines, here: ME })} machineId={id} />;
      }
      await renderApp(<Live />);
      return (id: string) => act(() => open(id));
    }

    it("is absent while no release is ready on the machine", async () => {
      await show(OTHER, { records: [at(ME, 1), at(OTHER, 1, { version: "0.9.1" })] });
      expect(screen.queryByRole("heading", { name: "Update" })).toBeNull();
    });

    it("says the tag is ready, and Install sends it and shows Restarting on a 202", async () => {
      const calls = mockFetch(json(202, {}));
      await show(OTHER, { records: ready(OTHER) });

      expect(screen.getByText("v0.9.1 is ready")).toBeDefined();
      fireEvent.click(screen.getByRole("button", { name: "Install" }));

      await screen.findByText(/^Restarting/);
      expect(calls).toEqual([{ url: `/api/machines/${OTHER}/update`, method: "POST", body: { tag: "v0.9.1" } }]);
      expect(screen.queryByRole("button", { name: "Install" })).toBeNull();
    });

    it("shows the checklist's text for a refusal, and Install again", async () => {
      mockFetch(json(503, { error: "laptop is offline" }));
      await show(OTHER, { records: ready(OTHER) });
      fireEvent.click(screen.getByRole("button", { name: "Install" }));
      await screen.findByText("Not sent: laptop is offline");
      expect(screen.getByRole("button", { name: "Install" })).toBeDefined();
    });

    /** Shows laptop's page over the samples the test can change, as the events socket would. */
    async function sampled(first: AnyRecord[]) {
      let change: (next: AnyRecord[]) => void = () => {};
      function Live() {
        const [records, setRecords] = useState(first);
        change = setRecords;
        const machines = fleet(records, limited, {}, [ME, OTHER]);
        return <SettingsMachine {...pageProps({ state: { ...emptyState, synced: true, settings: limited }, machines, here: ME })} machineId={OTHER} />;
      }
      await renderApp(<Live />);
      return (next: AnyRecord[]) => act(() => change(next));
    }

    it("drops the status of an older tag's install when a newer release is ready", async () => {
      mockFetch(json(202, {}));
      const next = await sampled(ready(OTHER));
      fireEvent.click(screen.getByRole("button", { name: "Install" }));
      await screen.findByText(/^Restarting/);
      next([at(ME, 1), at(OTHER, 2, { version: "0.9.1", update_ready: "v0.9.2" })]);
      expect(screen.getByText("v0.9.2 is ready")).toBeDefined();
      expect(screen.getByRole("button", { name: "Install" })).toBeDefined();
      expect(screen.queryByText(/^(Restarting|Running)/)).toBeNull();
    });

    it("goes once the machine's release is no longer ready", async () => {
      mockFetch(json(202, {}));
      const next = await sampled(ready(OTHER));
      fireEvent.click(screen.getByRole("button", { name: "Install" }));
      await screen.findByText(/^Restarting/);
      next([at(ME, 1), at(OTHER, 2, { version: "0.9.1" })]);
      expect(screen.queryByRole("heading", { name: "Update" })).toBeNull();
    });

    it("offers no Install while the page does not know which machine serves it", async () => {
      const calls = mockFetch(json(202, {}));
      const machines = fleet(ready(OTHER), limited, {}, [ME, OTHER]);
      await renderApp(<SettingsMachine {...pageProps({ state: { ...emptyState, synced: true, settings: limited }, machines, here: undefined })} machineId={OTHER} />);
      expect(screen.getByText("v0.9.1 is ready")).toBeDefined();
      const install = screen.queryByRole<HTMLButtonElement>("button", { name: "Install" });
      if (install) fireEvent.click(install);
      expect(install === null || install.disabled).toBe(true);
      expect(calls).toEqual([]);
    });

    it("says, before anything is sent, that the machine serving the page restarts", async () => {
      const calls = mockFetch();
      await show(ME, { records: ready(ME) });
      const update = screen.getByRole("region", { name: "Update" });
      expect(within(update).getByText(/this machine serves this page: it restarts/i)).toBeDefined();
      expect(calls).toEqual([]);
    });

    it("says nothing of a restart on another machine's page", async () => {
      await show(OTHER, { records: ready(OTHER) });
      expect(within(screen.getByRole("region", { name: "Update" })).queryByText(/serves this page/i)).toBeNull();
    });

    for (const [how, end] of [
      ["drops", (c: { answer: (r: Response) => void; drop: (e: Error) => void }) => c.drop(new TypeError("Failed to fetch"))],
      ["answers 504", (c: { answer: (r: Response) => void; drop: (e: Error) => void }) => c.answer(json(504, { error: "the link went down" }))],
    ] as const) {
      it(`offers no Install again on the serving machine when its call ${how}, since it may be restarting`, async () => {
        const out = pending();
        await show(ME, { records: ready(ME) });
        fireEvent.click(screen.getByRole("button", { name: "Install" }));
        // The serving machine's call goes last, once the others (none here) have answered.
        await waitFor(() => expect(out).toHaveLength(1));
        await act(async () => end(out[0]));
        expect(await screen.findByText(/^Sent, outcome unknown/)).toBeDefined();
        expect(screen.queryByRole("button", { name: "Install" })).toBeNull();
        expect(screen.getByText(/the machine serving this page may be restarting/i)).toBeDefined();
      });

      it(`offers Install again on another machine when its call ${how}`, async () => {
        const out = pending();
        await show(OTHER, { records: ready(OTHER) });
        fireEvent.click(screen.getByRole("button", { name: "Install" }));
        await act(async () => end(out[0]));
        expect(await screen.findByText(/^Sent, outcome unknown/)).toBeDefined();
        expect(screen.getByRole("button", { name: "Install" })).toBeDefined();
      });
    }

    it("shows another machine's page with none of the last machine's install", async () => {
      mockFetch(json(202, {}));
      const open = await both(OTHER);
      fireEvent.click(screen.getByRole("button", { name: "Install" }));
      await screen.findByText(/^Restarting/);
      // The serving machine has the same release ready, and nothing was sent to it.
      open(ME);
      expect(screen.queryByText(/^Restarting/)).toBeNull();
    });

    it("opens another machine's page with none of the last machine's unsaved edits", async () => {
      const open = await both(OTHER);
      type("Name", "desk");
      open(ME);
      expect(field("Name").value).toBe("studio");
      expect(bar()).toBeNull();
    });

    it("names the machine serving the page, not this one, when another machine's page is held by its restart", async () => {
      mockFetch(json(202, {}));
      const open = await both(ME);
      fireEvent.click(screen.getByRole("button", { name: "Install" }));
      await screen.findByText(/^Restarting/);
      open(OTHER);
      expect(screen.getByText(/the machine serving this page is restarting/i)).toBeDefined();
      expect(screen.queryByText(/^this machine is restarting/i)).toBeNull();
      expect(screen.queryByRole("button", { name: "Install" })).toBeNull();
    });

    it("offers no Install on the serving machine's page while an install to another machine is still out", async () => {
      const out = pending();
      const open = await both(OTHER);
      fireEvent.click(screen.getByRole("button", { name: "Install" }));
      open(ME);
      expect(screen.queryByRole("button", { name: "Install" })).toBeNull();
      expect(screen.getByText(/still out/i)).toBeDefined();
      await act(async () => out[0].answer(json(202, {})));
      expect(await screen.findByRole("button", { name: "Install" })).toBeDefined();
    });
  });

  describe("removing", () => {
    it("is absent on the machine serving the page", async () => {
      await show(ME);
      expect(screen.queryByRole("button", { name: "Remove from fleet" })).toBeNull();
      expect(screen.queryByRole("heading", { name: "Remove from the fleet" })).toBeNull();
    });

    it("is absent while the page does not know which machine serves it", async () => {
      const machines = fleet([at(ME, 1), at(OTHER, 1)], limited, {}, [ME, OTHER]);
      await renderApp(<SettingsMachine {...pageProps({ state: { ...emptyState, synced: true, settings: limited }, machines, here: undefined })} machineId={OTHER} />);
      expect(screen.queryByRole("button", { name: "Remove from fleet" })).toBeNull();
    });

    it("asks once more, with the name, and posts nothing until confirmed", async () => {
      const calls = mockFetch(new Response(null, { status: 204 }));
      await show(OTHER);

      fireEvent.click(screen.getByRole("button", { name: "Remove from fleet" }));
      const dialog = screen.getByRole("dialog", { name: "Remove laptop" });
      expect(calls).toEqual([]);
      fireEvent.click(within(dialog).getByRole("button", { name: "Remove from fleet" }));

      await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
      expect(calls).toEqual([{ url: `/api/machines/${OTHER}/remove`, method: "POST", body: undefined }]);
    });

    it("shows a refusal in the dialog and keeps it open", async () => {
      mockFetch(json(409, { error: "laptop is the acting coordinator" }));
      await show(OTHER);
      fireEvent.click(screen.getByRole("button", { name: "Remove from fleet" }));
      const dialog = screen.getByRole("dialog", { name: "Remove laptop" });
      fireEvent.click(within(dialog).getByRole("button", { name: "Remove from fleet" }));
      expect((await within(dialog).findByRole("alert")).textContent).toBe("laptop is the acting coordinator");
    });
  });

  it("says an unknown id is not in the fleet, with a link to Settings → Machines", async () => {
    await show("nope");
    expect(screen.getByText("No machine nope in this fleet")).toBeDefined();
    expect(screen.getByRole("link", { name: "Settings → Machines" }).getAttribute("href")).toBe("/settings/machines");
    expect(screen.queryByRole("textbox")).toBeNull();
  });
});
