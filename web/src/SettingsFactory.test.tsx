import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import type { QueryClient } from "@tanstack/react-query";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { FactoryOverview, FactorySettings } from "./api";
import { factoryKey } from "./factory";
import { SettingsFactory } from "./SettingsFactory";
import { json, mockFetch, pageProps, renderApp } from "./test-utils";

afterEach(() => vi.unstubAllGlobals());

const saved: FactorySettings = { paused: false, poll_seconds: 30, feedback_seconds: 300, max_runs: 3, max_gates: 2, model: "opus", effort: "high" };
const overview = (over: Partial<FactorySettings> = {}): FactoryOverview => ({ settings: { ...saved, ...over }, projects: [] });

const type = (label: string, value: string) => fireEvent.change(screen.getByLabelText(label), { target: { value } });
const bar = () => screen.queryByRole("region", { name: "Unsaved changes" });
/** The row of a field: what its label names, with its control and any note under it. */
const row = (label: string) => screen.getByLabelText(label).closest("[data-row]") as HTMLElement;

async function show(...answers: Response[]) {
  const calls = mockFetch(json(200, overview()), ...answers);
  const view = await renderApp(<SettingsFactory {...pageProps()} />);
  await screen.findByLabelText("Max runs");
  return { calls, client: view.client as QueryClient };
}
const overviewChanges = (client: QueryClient, over: Partial<FactorySettings>) =>
  act(async () => {
    client.setQueryData(factoryKey, overview(over));
    // The query client tells its observers on the next tick.
    await new Promise((resolve) => setTimeout(resolve, 0));
  });

describe("SettingsFactory", () => {
  it("shows today's fields from the overview, and no review bar until the first edit", async () => {
    await show();
    expect((screen.getByLabelText("Max runs") as HTMLInputElement).value).toBe("3");
    expect((screen.getByLabelText("Max gates") as HTMLInputElement).value).toBe("2");
    expect((screen.getByLabelText("Model") as HTMLInputElement).value).toBe("opus");
    expect((screen.getByLabelText("Pause claims") as HTMLInputElement).checked).toBe(false);
    expect(bar()).toBeNull();

    type("Max runs", "4");
    expect(bar()?.textContent).toContain("1 change");
    expect(bar()?.textContent).toContain("Runs at once 3 → 4");
    expect(row("Max runs").getAttribute("data-changed")).toBe("true");
    expect(row("Model").getAttribute("data-changed")).toBeNull();
  });

  it("is reachable by the label a row shows", async () => {
    await show();
    expect(screen.getByLabelText("Runs at once")).toBe(screen.getByLabelText("Max runs"));
  });

  it("sends one whole object, built from the overview plus the edits, and the bar leaves", async () => {
    const { calls } = await show(json(200, { settings: { ...saved, max_runs: 4, paused: true } }), json(200, overview({ max_runs: 4, paused: true })));
    fireEvent.click(screen.getByLabelText("Pause claims"));
    type("Max runs", "4");
    expect(bar()?.textContent).toContain("2 changes");
    expect(bar()?.textContent).toContain("Pause claims off → on");
    fireEvent.click(screen.getByRole("button", { name: "Save 2 changes" }));
    await waitFor(() => expect(bar()).toBeNull());
    const posts = calls.filter((c) => c.method === "POST");
    expect(posts).toEqual([
      {
        url: "/api/factory/settings",
        method: "POST",
        body: { paused: true, model: "opus", effort: "high", max_runs: 4, max_gates: 2, poll_seconds: 30, feedback_seconds: 300 },
      },
    ]);
    expect((screen.getByLabelText("Max runs") as HTMLInputElement).value).toBe("4");
  });

  it("refuses an out-of-range field before sending, and the refusal stays beside its field", async () => {
    const { calls } = await show();
    type("Poll seconds", "5");
    fireEvent.click(screen.getByRole("button", { name: "Save 1 change" }));
    await waitFor(() => expect(row("Poll seconds").textContent).toContain("poll_seconds must be a whole number from 10 to 60"));
    expect(bar()?.textContent).toContain("poll_seconds must be a whole number from 10 to 60");
    await act(() => new Promise((resolve) => setTimeout(resolve, 0)));
    expect(calls.filter((c) => c.method === "POST")).toHaveLength(0);
    expect(row("Max runs").textContent).not.toContain("must be");

    type("Poll seconds", "20");
    expect(row("Poll seconds").textContent).not.toContain("must be");
  });

  it("keeps the change and shows the daemon's refusal beside Save when it names no field", async () => {
    await show(json(409, { error: "the coordinator holds the settings" }));
    type("Max runs", "4");
    fireEvent.click(screen.getByRole("button", { name: "Save 1 change" }));
    const beside = await waitFor(() => {
      const found = bar()!.querySelector("[data-beside='save']")!;
      expect(found.textContent).toContain("the coordinator holds the settings");
      return found;
    });
    expect(beside).toBeDefined();
    expect(bar()?.textContent).toContain("Runs at once 3 → 4");
  });

  it("marks a row whose field changed elsewhere while it is being edited, and Save asks again", async () => {
    const { calls, client } = await show(json(200, { settings: { ...saved, max_runs: 5 } }), json(200, overview({ max_runs: 5 })));
    type("Max runs", "5");
    await overviewChanges(client, { max_runs: 4 });
    expect(row("Max runs").textContent).toContain("changed elsewhere");
    expect(row("Max runs").getAttribute("data-stale")).toBe("true");
    expect((screen.getByLabelText("Max runs") as HTMLInputElement).value).toBe("5");
    expect(bar()?.textContent).toContain("Runs at once 4 → 5");

    fireEvent.click(screen.getByRole("button", { name: "Look again" }));
    await act(() => new Promise((resolve) => setTimeout(resolve, 0)));
    expect(calls.filter((c) => c.method === "POST")).toHaveLength(0);

    fireEvent.click(screen.getByRole("button", { name: "Save 1 change" }));
    await waitFor(() => expect(calls.filter((c) => c.method === "POST")).toHaveLength(1));
    expect(calls.find((c) => c.method === "POST")?.body).toMatchObject({ max_runs: 5 });
  });

  it("does not mark a field that changed elsewhere while it was untouched", async () => {
    const { client } = await show();
    await overviewChanges(client, { max_runs: 4 });
    expect(row("Max runs").getAttribute("data-stale")).toBeNull();
    expect(bar()).toBeNull();
  });

  it("lets untouched fields follow the overview, and keeps what was typed", async () => {
    const { client } = await show();
    type("Max gates", "9");
    await overviewChanges(client, { model: "sonnet", max_gates: 1, max_runs: 6 });
    expect((screen.getByLabelText("Model") as HTMLInputElement).value).toBe("sonnet");
    expect((screen.getByLabelText("Max runs") as HTMLInputElement).value).toBe("6");
    expect((screen.getByLabelText("Max gates") as HTMLInputElement).value).toBe("9");
    expect(bar()?.textContent).toContain("Gates at once 1 → 9");
    expect(bar()?.textContent).not.toContain("Runs at once");
  });

  it("puts back the newest saved values on Discard", async () => {
    const { client } = await show();
    type("Max gates", "9");
    await overviewChanges(client, { max_gates: 1 });
    fireEvent.click(screen.getByRole("button", { name: "Discard" }));
    expect(bar()).toBeNull();
    expect((screen.getByLabelText("Max gates") as HTMLInputElement).value).toBe("1");
  });

  it("treats 05 as 5, so no change is listed", async () => {
    await show();
    type("Max runs", "03");
    expect(bar()).toBeNull();
  });

  it("lists a change in the bar under its row label", async () => {
    await show();
    type("Model", "sonnet");
    expect(within(bar()!).getByText("Model opus → sonnet")).toBeDefined();
  });
});
