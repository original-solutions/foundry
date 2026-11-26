import { fireEvent, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { FactorySettings } from "./api";
import { FleetSettings, ProjectFactoryControls } from "./FactorySettings";
import { json, mockFetch, renderApp } from "./test-utils";

afterEach(() => vi.unstubAllGlobals());

const current: FactorySettings = { paused: false, poll_seconds: 30, feedback_seconds: 300, max_runs: 2, max_gates: 3, model: "opus", effort: "high" };

const type = (label: string, value: string) => fireEvent.change(screen.getByLabelText(label), { target: { value } });
const save = () => fireEvent.click(screen.getByRole("button", { name: "Save" }));

describe("FleetSettings", () => {
  it.each([
    ["Poll seconds", "5", ["10", "60"]],
    ["Poll seconds", "61", ["10", "60"]],
    ["Max runs", "-1", ["0", "4294967295"]],
    ["Max gates", "1.5", ["whole number"]],
    ["Feedback seconds", "", ["whole number"]],
    ["Model", "-x", ["one word"]],
    ["Effort", "two words", ["one word"]],
  ])("refuses %s of %s in the form and sends nothing", async (label, value, shows) => {
    const calls = mockFetch();
    await renderApp(<FleetSettings settings={current} />);
    type(label, value);
    save();
    const alert = screen.getByRole("alert");
    for (const text of shows) expect(alert.textContent).toContain(text);
    expect(calls).toHaveLength(0);
  });

  it("sends every field once, numbers as numbers, when all are in range", async () => {
    const calls = mockFetch(json(200, { settings: current }));
    await renderApp(<FleetSettings settings={current} />);
    fireEvent.click(screen.getByLabelText("Pause claims"));
    type("Model", "sonnet");
    type("Effort", "low");
    type("Max runs", "4");
    type("Max gates", "0");
    type("Poll seconds", "10");
    type("Feedback seconds", "60");
    save();
    await waitFor(() => expect(calls).toHaveLength(1));
    expect(calls[0]).toEqual({
      url: "/api/factory/settings",
      method: "POST",
      body: { paused: true, model: "sonnet", effort: "low", max_runs: 4, max_gates: 0, poll_seconds: 10, feedback_seconds: 60 },
    });
  });

  it("shows the daemon's refusal", async () => {
    mockFetch(json(422, { error: "poll_seconds is out of range" }));
    await renderApp(<FleetSettings settings={current} />);
    save();
    expect((await screen.findByRole("alert")).textContent).toContain("poll_seconds is out of range");
  });
});

describe("ProjectFactoryControls", () => {
  const project = { id: "p1", name: "demo", repo: "https://github.com/acme/demo.git" };

  it("sends the whole setting when the switch is flipped", async () => {
    const calls = mockFetch(json(200, { factory: { on: true, login: "", paused: false } }));
    await renderApp(<ProjectFactoryControls project={project} factory={{ on: false, login: "", paused: false }} />);
    const toggle = screen.getByRole("switch", { name: "Factory for demo" });
    expect(toggle.getAttribute("aria-checked")).toBe("false");
    fireEvent.click(toggle);
    await waitFor(() => expect(calls).toHaveLength(1));
    expect(calls[0]).toEqual({ url: "/api/projects/p1/factory", method: "POST", body: { on: true, paused: false, login: null } });
  });

  it("keeps the held login and sends a typed one with Save login", async () => {
    const calls = mockFetch(json(200, { factory: {} }));
    await renderApp(<ProjectFactoryControls project={project} factory={{ on: true, login: "bot", paused: false }} />);
    expect((screen.getByLabelText("GitHub login for demo") as HTMLInputElement).value).toBe("bot");
    expect(screen.queryByRole("button", { name: "Save login" })).toBeNull();
    fireEvent.change(screen.getByLabelText("GitHub login for demo"), { target: { value: " other " } });
    fireEvent.click(screen.getByRole("button", { name: "Save login" }));
    await waitFor(() => expect(calls).toHaveLength(1));
    expect(calls[0].body).toEqual({ on: true, paused: false, login: "other" });
  });

  it("shows a refusal beside the row", async () => {
    mockFetch(json(422, { error: "only https://github.com repos are in scope" }));
    await renderApp(<ProjectFactoryControls project={project} factory={{ on: false, login: "", paused: false }} />);
    fireEvent.click(screen.getByRole("switch", { name: "Factory for demo" }));
    expect((await screen.findByRole("alert")).textContent).toContain("only https://github.com repos are in scope");
  });
});
