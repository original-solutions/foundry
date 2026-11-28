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
/** What the daemon puts in place of each string of an answer when its secret store cannot be read. */
const HIDDEN = "[hidden: the secret store could not be read]";

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
    expect(bar()?.textContent).not.toContain("must be");
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

  it("sends a model typed with a space after it as one word", async () => {
    const { calls } = await show(json(200, { settings: { ...saved, model: "sonnet" } }), json(200, overview({ model: "sonnet" })));
    type("Model", "sonnet ");
    fireEvent.click(screen.getByRole("button", { name: "Save 1 change" }));
    await waitFor(() => expect(calls.filter((c) => c.method === "POST")).toHaveLength(1));
    expect(calls.find((c) => c.method === "POST")?.body).toMatchObject({ model: "sonnet" });
  });

  it("clears the stale mark when the field is edited again after it went stale", async () => {
    const { client } = await show();
    type("Max runs", "5");
    await overviewChanges(client, { max_runs: 4 });
    expect(row("Max runs").getAttribute("data-stale")).toBe("true");
    type("Max runs", "6");
    expect(row("Max runs").getAttribute("data-stale")).toBeNull();
    expect(screen.getByRole("button", { name: "Save 1 change" })).toBeDefined();
  });

  it("asks again after a look when the field moves again, and the look lets the row stop being stale", async () => {
    const { calls, client } = await show(json(200, { settings: { ...saved, max_runs: 5 } }), json(200, overview({ max_runs: 5 })));
    type("Max runs", "5");
    await overviewChanges(client, { max_runs: 4 });
    fireEvent.click(screen.getByRole("button", { name: "Look again" }));
    expect(row("Max runs").getAttribute("data-stale")).toBeNull();
    expect(screen.getByRole("button", { name: "Save 1 change" })).toBeDefined();

    await overviewChanges(client, { max_runs: 7 });
    expect(row("Max runs").getAttribute("data-stale")).toBe("true");
    expect(screen.getByRole("button", { name: "Look again" })).toBeDefined();
    await act(() => new Promise((resolve) => setTimeout(resolve, 0)));
    expect(calls.filter((c) => c.method === "POST")).toHaveLength(0);
  });

  it("asks again when a second, different field goes stale after a look", async () => {
    const { client } = await show();
    type("Max runs", "5");
    type("Max gates", "9");
    await overviewChanges(client, { max_runs: 4 });
    fireEvent.click(screen.getByRole("button", { name: "Look again" }));
    expect(screen.getByRole("button", { name: "Save 2 changes" })).toBeDefined();
    await overviewChanges(client, { max_runs: 4, max_gates: 1 });
    expect(row("Max gates").getAttribute("data-stale")).toBe("true");
    expect(screen.getByRole("button", { name: "Look again" })).toBeDefined();
  });

  it("keeps what was typed when the answer hides model and effort, and shows no hidden text", async () => {
    const { calls } = await show(json(200, { settings: { ...saved, model: HIDDEN, effort: HIDDEN, max_runs: 4 } }), json(200, overview({ model: HIDDEN, effort: HIDDEN, max_runs: 4 })));
    type("Model", "sonnet");
    type("Effort", "low");
    type("Max runs", "4");
    fireEvent.click(screen.getByRole("button", { name: "Save 3 changes" }));
    await waitFor(() => expect(bar()).toBeNull());
    expect(calls.find((c) => c.method === "POST")?.body).toMatchObject({ model: "sonnet", effort: "low" });
    expect((screen.getByLabelText("Model") as HTMLInputElement).value).toBe("sonnet");
    expect((screen.getByLabelText("Effort") as HTMLInputElement).value).toBe("low");
    expect(document.body.textContent).not.toContain("[hidden");
    expect(Array.from(document.querySelectorAll("input")).some((i) => i.value.includes("[hidden"))).toBe(false);
  });

  it("releases a field typed back to its saved value, so it follows the overview again", async () => {
    const { client } = await show();
    type("Max runs", "5");
    type("Max runs", "3");
    expect(bar()).toBeNull();
    await overviewChanges(client, { max_runs: 6 });
    expect((screen.getByLabelText("Max runs") as HTMLInputElement).value).toBe("6");
    expect(bar()).toBeNull();
  });

  it("follows the overview after a save, with no bar for a field that was saved", async () => {
    const { client } = await show(json(200, { settings: { ...saved, max_runs: 5 } }), json(200, overview({ max_runs: 5 })));
    type("Max runs", "5");
    fireEvent.click(screen.getByRole("button", { name: "Save 1 change" }));
    await waitFor(() => expect(bar()).toBeNull());
    await overviewChanges(client, { max_runs: 4 });
    expect((screen.getByLabelText("Max runs") as HTMLInputElement).value).toBe("4");
    expect(bar()).toBeNull();
  });

  it("keeps an edit typed while the save is in flight, and does not mark it as changed elsewhere by the user's own save", async () => {
    let answer: (r: Response) => void = () => {};
    const pending = new Promise<Response>((resolve) => (answer = resolve));
    mockFetch(json(200, overview()));
    const fetchMock = globalThis.fetch as unknown as ReturnType<typeof vi.fn<(url: string, init?: RequestInit) => Promise<Response>>>;
    const first = fetchMock.getMockImplementation()!;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => (init?.method === "POST" ? pending : first(url, init)));
    const view = await renderApp(<SettingsFactory {...pageProps()} />);
    await screen.findByLabelText("Max runs");

    type("Max runs", "5");
    fireEvent.click(screen.getByRole("button", { name: "Save 1 change" }));
    type("Max runs", "6");
    await act(async () => answer(json(200, { settings: { ...saved, max_runs: 5 } })));
    await act(async () => {
      view.client.setQueryData(factoryKey, overview({ max_runs: 5 }));
      await new Promise((resolve) => setTimeout(resolve, 0));
    });
    expect((screen.getByLabelText("Max runs") as HTMLInputElement).value).toBe("6");
    expect(bar()?.textContent).toContain("Runs at once 5 → 6");
    expect(row("Max runs").getAttribute("data-stale")).toBeNull();
    expect(screen.getByRole("button", { name: "Save 1 change" })).toBeDefined();
  });

  it("sends an effort typed with a space after it as one word", async () => {
    const { calls } = await show(json(200, { settings: { ...saved, effort: "low" } }), json(200, overview({ effort: "low" })));
    type("Effort", "low ");
    fireEvent.click(screen.getByRole("button", { name: "Save 1 change" }));
    await waitFor(() => expect(calls.filter((c) => c.method === "POST")).toHaveLength(1));
    expect(calls.find((c) => c.method === "POST")?.body).toMatchObject({ effort: "low" });
  });

  it("follows the overview as read again after a save, whatever the save answered", async () => {
    await show(json(200, { settings: { ...saved, max_runs: 5 } }), json(200, overview({ max_runs: 7 })));
    type("Max runs", "5");
    fireEvent.click(screen.getByRole("button", { name: "Save 1 change" }));
    await waitFor(() => expect((screen.getByLabelText("Max runs") as HTMLInputElement).value).toBe("7"));
    expect(bar()).toBeNull();
  });

  it("shows no hidden text for an overview whose model the daemon could not read", async () => {
    mockFetch(json(200, overview({ model: HIDDEN, effort: HIDDEN })));
    await renderApp(<SettingsFactory {...pageProps()} />);
    await screen.findByLabelText("Max runs");
    expect(document.body.textContent).not.toContain("[hidden");
    expect(Array.from(document.querySelectorAll("input")).some((i) => i.value.includes("[hidden"))).toBe(false);
    expect(bar()).toBeNull();
  });

  it("keeps an edit typed back to the old value while its save is in flight", async () => {
    let answer: (r: Response) => void = () => {};
    const pending = new Promise<Response>((resolve) => (answer = resolve));
    mockFetch(json(200, overview()));
    const fetchMock = globalThis.fetch as unknown as ReturnType<typeof vi.fn<(url: string, init?: RequestInit) => Promise<Response>>>;
    const first = fetchMock.getMockImplementation()!;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => (init?.method === "POST" ? pending : first(url, init)));
    const view = await renderApp(<SettingsFactory {...pageProps()} />);
    await screen.findByLabelText("Max runs");

    type("Max runs", "4");
    fireEvent.click(screen.getByRole("button", { name: "Save 1 change" }));
    type("Max runs", "3");
    await act(async () => answer(json(200, { settings: { ...saved, max_runs: 4 } })));
    await act(async () => {
      view.client.setQueryData(factoryKey, overview({ max_runs: 4 }));
      await new Promise((resolve) => setTimeout(resolve, 0));
    });
    expect((screen.getByLabelText("Max runs") as HTMLInputElement).value).toBe("3");
    expect(bar()?.textContent).toContain("Runs at once 4 → 3");
  });
});
