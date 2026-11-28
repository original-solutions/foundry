import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AnyRecord, MachineSample, SharedSettings } from "./api";
import { emptyState, fleet } from "./fleet";
import { SettingsMachine } from "./SettingsMachine";
import { ME, OTHER, sample, settings } from "./test-fixtures";
import { json, mockFetch, pageProps, renderApp } from "./test-utils";

afterEach(() => vi.unstubAllGlobals());

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

const type = (label: string, value: string) => fireEvent.change(screen.getByLabelText(label), { target: { value } });
const bar = () => screen.queryByRole("region", { name: "Unsaved changes" });
const field = (label: string) => screen.getByLabelText<HTMLInputElement>(label);

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
    const calls = mockFetch(new Response(null, { status: 204 }), json(422, { error: "max_sessions must be at least 1" }));
    await show(OTHER);

    type("Name", "desk");
    type("Max sessions", "0");
    fireEvent.click(screen.getByRole("button", { name: "Save 2 changes" }));

    await waitFor(() => expect(bar()?.textContent).toContain("max_sessions must be at least 1"));
    expect(bar()?.textContent).toContain("1 change");
    expect(bar()?.textContent).toContain("Max sessions 4 → 0");
    expect(bar()?.textContent).not.toContain("Name laptop");
    expect(calls).toHaveLength(2);
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

  describe("the update", () => {
    it("is absent while no release is ready on the machine", async () => {
      await show(OTHER, { records: [at(ME, 1), at(OTHER, 1, { version: "0.9.1" })] });
      expect(screen.queryByRole("heading", { name: "Update" })).toBeNull();
    });

    it("says the tag is ready, and Install sends it and shows Restarting on a 202", async () => {
      const calls = mockFetch(json(202, {}));
      await show(OTHER, { records: [at(ME, 1), at(OTHER, 1, { version: "0.9.0", update_ready: "v0.9.1" })] });

      expect(screen.getByText("v0.9.1 is ready")).toBeDefined();
      fireEvent.click(screen.getByRole("button", { name: "Install" }));

      await screen.findByText(/^Restarting/);
      expect(calls).toEqual([{ url: `/api/machines/${OTHER}/update`, method: "POST", body: { tag: "v0.9.1" } }]);
      expect(screen.queryByRole("button", { name: "Install" })).toBeNull();
    });

    it("shows the checklist's text for a refusal, and Install again", async () => {
      mockFetch(json(503, { error: "laptop is offline" }));
      await show(OTHER, { records: [at(ME, 1), at(OTHER, 1, { version: "0.9.0", update_ready: "v0.9.1" })] });
      fireEvent.click(screen.getByRole("button", { name: "Install" }));
      await screen.findByText("Not sent: laptop is offline");
      expect(screen.getByRole("button", { name: "Install" })).toBeDefined();
    });

    it("says, before anything is sent, that the machine serving the page restarts", async () => {
      const calls = mockFetch();
      await show(ME, { records: [at(ME, 1, { version: "0.9.0", update_ready: "v0.9.1" }), at(OTHER, 1)] });
      const update = screen.getByRole("region", { name: "Update" });
      expect(within(update).getByText(/this machine serves this page: it restarts/i)).toBeDefined();
      expect(calls).toEqual([]);
    });

    it("says nothing of a restart on another machine's page", async () => {
      await show(OTHER, { records: [at(ME, 1), at(OTHER, 1, { version: "0.9.0", update_ready: "v0.9.1" })] });
      expect(within(screen.getByRole("region", { name: "Update" })).queryByText(/serves this page/i)).toBeNull();
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
    expect(screen.queryByLabelText("Name")).toBeNull();
  });
});
