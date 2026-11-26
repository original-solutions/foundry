import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { FactorySettings } from "./api";
import { FleetSettings, ProjectFactoryControls } from "./FactorySettings";
import { json, mockFetch, renderApp } from "./test-utils";

/** What the daemon puts in place of each string of an answer when its secret store cannot be read (`secrets::HIDDEN_UNREADABLE`). */
const HIDDEN = "[hidden: the secret store could not be read]";

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
    // A send starts a microtask later; wait for it before saying nothing was sent.
    await act(() => new Promise((resolve) => setTimeout(resolve, 0)));
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

  it("follows new settings while untouched, and keeps what was typed", async () => {
    let set: (s: FactorySettings) => void = () => {};
    function Harness() {
      const [s, setS] = useState(current);
      set = setS;
      return <FleetSettings settings={s} />;
    }
    mockFetch();
    await renderApp(<Harness />);
    act(() => set({ ...current, max_runs: 4, model: "sonnet" }));
    expect((screen.getByLabelText("Max runs") as HTMLInputElement).value).toBe("4");
    expect((screen.getByLabelText("Model") as HTMLInputElement).value).toBe("sonnet");

    type("Effort", "low");
    act(() => set({ ...current, max_runs: 1, effort: "max" }));
    expect((screen.getByLabelText("Effort") as HTMLInputElement).value).toBe("low");
  });

  it("lets the fields not being edited follow new settings, so a save sends them as they now are", async () => {
    let set: (s: FactorySettings) => void = () => {};
    function Harness() {
      const [s, setS] = useState(current);
      set = setS;
      return <FleetSettings settings={s} />;
    }
    const calls = mockFetch(json(200, { settings: { ...current, model: "sonnet", max_runs: 4 } }));
    await renderApp(<Harness />);
    type("Max runs", "4");
    act(() => set({ ...current, model: "sonnet", max_runs: 3 }));
    expect((screen.getByLabelText("Model") as HTMLInputElement).value).toBe("sonnet");
    expect((screen.getByLabelText("Max runs") as HTMLInputElement).value).toBe("4");
    save();
    await waitFor(() => expect(calls).toHaveLength(1));
    expect(calls[0].body).toEqual({ ...current, model: "sonnet", max_runs: 4 });
  });

  it("is clean again after a save, whatever way a number was typed", async () => {
    let set: (s: FactorySettings) => void = () => {};
    function Harness() {
      const [s, setS] = useState(current);
      set = setS;
      return <FleetSettings settings={s} />;
    }
    mockFetch(json(200, { settings: { ...current, max_runs: 5 } }));
    await renderApp(<Harness />);
    type("Max runs", "05");
    save();
    await waitFor(() => expect((screen.getByLabelText("Max runs") as HTMLInputElement).value).toBe("5"));
    act(() => set({ ...current, max_runs: 6 }));
    expect((screen.getByLabelText("Max runs") as HTMLInputElement).value).toBe("6");
  });

  it("keeps a model typed while a save is on its way", async () => {
    let answer: (r: Response) => void = () => {};
    const later = new Promise<Response>((resolve) => (answer = resolve));
    mockFetch(later as unknown as Response);
    await renderApp(<FleetSettings settings={current} />);
    type("Model", "sonnet");
    save();
    type("Model", "haiku");
    await act(async () => answer(json(200, { settings: { ...current, model: "sonnet" } })));
    // Let the save's success handler run before looking.
    await act(() => new Promise((resolve) => setTimeout(resolve, 20)));
    expect((screen.getByLabelText("Model") as HTMLInputElement).value).toBe("haiku");
  });

  it("keeps the submitted model and effort when the daemon answers its placeholder for strings it could not read", async () => {
    // The daemon hides every string of an answer at once when its secret store cannot be read.
    mockFetch(json(200, { settings: { ...current, model: HIDDEN, effort: HIDDEN } }));
    await renderApp(<FleetSettings settings={current} />);
    type("Model", "sonnet");
    type("Effort", "medium");
    save();
    await act(() => new Promise((resolve) => setTimeout(resolve, 20)));
    expect((screen.getByLabelText("Model") as HTMLInputElement).value).toBe("sonnet");
    expect((screen.getByLabelText("Effort") as HTMLInputElement).value).toBe("medium");
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

  const idle = () => new Promise((resolve) => setTimeout(resolve, 0));

  it("builds a second change from the daemon's answer while the props are stale", async () => {
    const calls = mockFetch(json(200, { factory: { on: true, login: "bot", paused: false } }), json(200, { factory: { on: true, login: "bot", paused: true } }));
    await renderApp(<ProjectFactoryControls project={project} factory={{ on: false, login: "", paused: false }} />);
    fireEvent.click(screen.getByRole("switch", { name: "Factory for demo" }));
    await waitFor(() => expect(screen.getByRole("switch", { name: "Factory for demo" }).getAttribute("aria-checked")).toBe("true"));
    await waitFor(() => expect((screen.getByLabelText("Pause claims for demo") as HTMLInputElement).disabled).toBe(false));
    fireEvent.click(screen.getByLabelText("Pause claims for demo"));
    await waitFor(() => expect(calls).toHaveLength(2));
    expect(calls[1].body).toEqual({ on: true, paused: true, login: "bot" });
  });

  it("keeps a saved login after Save login while the props are stale", async () => {
    const calls = mockFetch(json(200, { factory: { on: true, login: "bot2", paused: false } }), json(200, { factory: { on: true, login: "bot2", paused: true } }));
    await renderApp(<ProjectFactoryControls project={project} factory={{ on: true, login: "bot", paused: false }} />);
    fireEvent.change(screen.getByLabelText("GitHub login for demo"), { target: { value: "bot2" } });
    fireEvent.click(screen.getByRole("button", { name: "Save login" }));
    await waitFor(() => expect(screen.queryByRole("button", { name: "Save login" })).toBeNull());
    expect((screen.getByLabelText("GitHub login for demo") as HTMLInputElement).value).toBe("bot2");
    await waitFor(() => expect((screen.getByLabelText("Pause claims for demo") as HTMLInputElement).disabled).toBe(false));
    fireEvent.click(screen.getByLabelText("Pause claims for demo"));
    await waitFor(() => expect(calls).toHaveLength(2));
    expect(calls[1].body).toEqual({ on: true, paused: true, login: "bot2" });
  });

  it("sends the saved login, never one typed and not saved, from the switch and the pause", async () => {
    const calls = mockFetch(json(200, { factory: { on: true, login: "bot", paused: true } }));
    await renderApp(<ProjectFactoryControls project={project} factory={{ on: true, login: "bot", paused: false }} />);
    fireEvent.change(screen.getByLabelText("GitHub login for demo"), { target: { value: "other" } });
    fireEvent.click(screen.getByLabelText("Pause claims for demo"));
    await waitFor(() => expect(calls).toHaveLength(1));
    expect(calls[0].body).toEqual({ on: true, paused: true, login: "bot" });
    await act(idle);
  });

  it("keeps a login typed and never saved when another change succeeds", async () => {
    mockFetch(json(200, { factory: { on: true, login: "bot", paused: true } }));
    await renderApp(<ProjectFactoryControls project={project} factory={{ on: true, login: "bot", paused: false }} />);
    fireEvent.change(screen.getByLabelText("GitHub login for demo"), { target: { value: "other" } });
    fireEvent.click(screen.getByLabelText("Pause claims for demo"));
    await waitFor(() => expect((screen.getByLabelText("Pause claims for demo") as HTMLInputElement).checked).toBe(true));
    await waitFor(() => expect((screen.getByLabelText("Pause claims for demo") as HTMLInputElement).disabled).toBe(false));
    expect((screen.getByLabelText("GitHub login for demo") as HTMLInputElement).value).toBe("other");
    expect(screen.getByRole("button", { name: "Save login" })).toBeDefined();
  });

  it("keeps the value last sent through settings that have not caught up, and drops it once they move", async () => {
    let set: (f: { on: boolean; login: string; paused: boolean }) => void = () => {};
    function Harness() {
      const [f, setF] = useState({ on: false, login: "", paused: false });
      set = setF;
      return <ProjectFactoryControls project={project} factory={f} />;
    }
    const calls = mockFetch(json(200, { factory: { on: true, login: "bot", paused: false } }), json(200, { factory: { on: false, login: "bot", paused: true } }));
    await renderApp(<Harness />);
    const toggle = () => screen.getByRole("switch", { name: "Factory for demo" });
    fireEvent.click(toggle());
    await waitFor(() => expect(toggle().getAttribute("aria-checked")).toBe("true"));
    // A settings message from before the edit was numbered: a new object, the same setting.
    act(() => set({ on: false, login: "", paused: false }));
    expect(toggle().getAttribute("aria-checked")).toBe("true");
    // The edit arrives, then someone else turns the factory off.
    act(() => set({ on: true, login: "bot", paused: false }));
    act(() => set({ on: false, login: "bot", paused: false }));
    expect(toggle().getAttribute("aria-checked")).toBe("false");
    await waitFor(() => expect((screen.getByLabelText("Pause claims for demo") as HTMLInputElement).disabled).toBe(false));
    fireEvent.click(screen.getByLabelText("Pause claims for demo"));
    await waitFor(() => expect(calls).toHaveLength(2));
    expect(calls[1].body).toEqual({ on: false, paused: true, login: "bot" });
  });

  it("keeps login typed while Save login is on its way, and clears what the save sent", async () => {
    let answer: (r: Response) => void = () => {};
    const later = new Promise<Response>((resolve) => (answer = resolve));
    mockFetch(later as unknown as Response);
    await renderApp(<ProjectFactoryControls project={project} factory={{ on: true, login: "bot", paused: false }} />);
    const box = () => screen.getByLabelText("GitHub login for demo") as HTMLInputElement;
    fireEvent.change(box(), { target: { value: "bot2" } });
    fireEvent.click(screen.getByRole("button", { name: "Save login" }));
    fireEvent.change(box(), { target: { value: "bot3" } });
    await act(async () => answer(json(200, { factory: { on: true, login: "bot2", paused: false } })));
    await act(() => new Promise((resolve) => setTimeout(resolve, 20)));
    expect(box().value).toBe("bot3");
    expect(screen.getByRole("button", { name: "Save login" })).toBeDefined();
  });

  it("clears what Save login sent, so the login the settings then hold shows", async () => {
    let set: (f: { on: boolean; login: string; paused: boolean }) => void = () => {};
    function Harness() {
      const [f, setF] = useState({ on: true, login: "bot", paused: false });
      set = setF;
      return <ProjectFactoryControls project={project} factory={f} />;
    }
    mockFetch(json(200, { factory: { on: true, login: "bot2", paused: false } }));
    await renderApp(<Harness />);
    const box = () => screen.getByLabelText("GitHub login for demo") as HTMLInputElement;
    fireEvent.change(box(), { target: { value: "bot2" } });
    fireEvent.click(screen.getByRole("button", { name: "Save login" }));
    await waitFor(() => expect(screen.queryByRole("button", { name: "Save login" })).toBeNull());
    act(() => set({ on: true, login: "bot3", paused: false }));
    expect(box().value).toBe("bot3");
    expect(screen.queryByRole("button", { name: "Save login" })).toBeNull();
  });

  it("keeps a chain of changes through settings that catch up one at a time", async () => {
    let set: (f: { on: boolean; login: string; paused: boolean }) => void = () => {};
    function Harness() {
      const [f, setF] = useState({ on: false, login: "", paused: false });
      set = setF;
      return <ProjectFactoryControls project={project} factory={f} />;
    }
    const a1 = { on: true, login: "bot", paused: false };
    const a2 = { on: true, login: "bot", paused: true };
    mockFetch(json(200, { factory: a1 }), json(200, { factory: a2 }));
    await renderApp(<Harness />);
    const toggle = () => screen.getByRole("switch", { name: "Factory for demo" });
    const pause = () => screen.getByLabelText("Pause claims for demo") as HTMLInputElement;
    fireEvent.click(toggle());
    await waitFor(() => expect(toggle().getAttribute("aria-checked")).toBe("true"));
    await waitFor(() => expect(pause().disabled).toBe(false));
    fireEvent.click(pause());
    await waitFor(() => expect(pause().checked).toBe(true));
    act(() => set(a1));
    expect(pause().checked).toBe(true);
    act(() => set(a2));
    expect(pause().checked).toBe(true);
    act(() => set({ on: false, login: "x", paused: false }));
    expect(toggle().getAttribute("aria-checked")).toBe("false");
    expect(pause().checked).toBe(false);
  });

  type Setting = { on: boolean; login: string; paused: boolean };
  /** The controls under a harness that moves their props, and handles on what they show. */
  function controls(first: Setting) {
    const hold: { set: (f: Setting) => void } = { set: () => {} };
    function Harness() {
      const [f, setF] = useState(first);
      hold.set = setF;
      return <ProjectFactoryControls project={project} factory={f} />;
    }
    return {
      Harness,
      set: (f: Setting) => act(() => hold.set(f)),
      toggle: () => screen.getByRole("switch", { name: "Factory for demo" }),
      pause: () => screen.getByLabelText("Pause claims for demo") as HTMLInputElement,
      box: () => screen.getByLabelText("GitHub login for demo") as HTMLInputElement,
      ready: () => waitFor(() => expect((screen.getByLabelText("Pause claims for demo") as HTMLInputElement).disabled).toBe(false)),
    };
  }

  it("keeps a chain whose answers come in sorted key order, as the daemon's do", async () => {
    const c = controls({ on: false, login: "", paused: false });
    mockFetch(json(200, { factory: { login: "bot", on: true, paused: false } }), json(200, { factory: { login: "bot", on: true, paused: true } }));
    await renderApp(<c.Harness />);
    fireEvent.click(c.toggle());
    await waitFor(() => expect(c.toggle().getAttribute("aria-checked")).toBe("true"));
    await c.ready();
    fireEvent.click(c.pause());
    await waitFor(() => expect(c.pause().checked).toBe(true));
    c.set({ on: true, login: "bot", paused: false });
    expect(c.pause().checked).toBe(true);
    c.set({ on: true, login: "bot", paused: true });
    expect(c.pause().checked).toBe(true);
    c.set({ on: true, login: "bot", paused: false });
    expect(c.pause().checked).toBe(false);
  });

  it("keeps the answer of a change still pending when the settings catch up to the one before", async () => {
    let answer: (r: Response) => void = () => {};
    const later = new Promise<Response>((resolve) => (answer = resolve));
    const c = controls({ on: false, login: "", paused: false });
    const calls = mockFetch(json(200, { factory: { on: true, login: "bot", paused: false } }), later as unknown as Response, json(200, { factory: { on: true, login: "bot", paused: false } }));
    await renderApp(<c.Harness />);
    fireEvent.click(c.toggle());
    await waitFor(() => expect(c.toggle().getAttribute("aria-checked")).toBe("true"));
    await c.ready();
    fireEvent.click(c.pause());
    c.set({ on: true, login: "bot", paused: false });
    await act(async () => answer(json(200, { factory: { on: true, login: "bot", paused: true } })));
    await c.ready();
    expect(c.pause().checked).toBe(true);
    fireEvent.click(c.pause());
    await waitFor(() => expect(calls).toHaveLength(3));
    expect(calls[2].body).toEqual({ on: true, paused: false, login: "bot" });
  });

  it("follows settings that go back to an earlier setting after two changes", async () => {
    const c = controls({ on: true, login: "bot", paused: false });
    mockFetch(json(200, { factory: { on: true, login: "bot", paused: true } }), json(200, { factory: { on: true, login: "bot", paused: false } }));
    await renderApp(<c.Harness />);
    fireEvent.click(c.pause());
    await waitFor(() => expect(c.pause().checked).toBe(true));
    await c.ready();
    fireEvent.click(c.pause());
    await waitFor(() => expect(c.pause().checked).toBe(false));
    c.set({ on: true, login: "bot", paused: true });
    c.set({ on: true, login: "bot", paused: false });
    c.set({ on: true, login: "bot", paused: true });
    expect(c.pause().checked).toBe(true);
  });

  it("shows the login the coordinator picked after a blank login is saved", async () => {
    const c = controls({ on: true, login: "bot", paused: false });
    mockFetch(json(200, { factory: { on: true, login: "fac-default", paused: false } }));
    await renderApp(<c.Harness />);
    fireEvent.change(c.box(), { target: { value: "" } });
    fireEvent.click(screen.getByRole("button", { name: "Save login" }));
    await waitFor(() => expect(screen.queryByRole("button", { name: "Save login" })).toBeNull());
    expect(c.box().value).toBe("fac-default");
  });

  it("never shows or sends the daemon's placeholder for a login it could not read", async () => {
    const c = controls({ on: false, login: "bot", paused: false });
    const calls = mockFetch(json(200, { factory: { on: true, login: HIDDEN, paused: false } }), json(200, { factory: { on: false, login: "bot", paused: true } }));
    await renderApp(<c.Harness />);
    fireEvent.click(c.toggle());
    await waitFor(() => expect(calls).toHaveLength(1));
    await c.ready();
    expect(c.box().value).toBe("bot");
    // Only strings are hidden: the switch the daemon confirmed stays on, so the next change keeps it.
    expect(c.toggle().getAttribute("aria-checked")).toBe("true");
    fireEvent.click(c.pause());
    await waitFor(() => expect(calls).toHaveLength(2));
    expect(calls[1].body).toEqual({ on: true, paused: true, login: "bot" });
  });

  it("keeps the login Save login sent, not the one held before, when the daemon answers its placeholder", async () => {
    const c = controls({ on: true, login: "bot", paused: false });
    const calls = mockFetch(json(200, { factory: { on: true, login: HIDDEN, paused: false } }), json(200, { factory: { on: true, login: "bot2", paused: true } }));
    await renderApp(<c.Harness />);
    fireEvent.change(c.box(), { target: { value: "bot2" } });
    fireEvent.click(screen.getByRole("button", { name: "Save login" }));
    await waitFor(() => expect(calls).toHaveLength(1));
    await c.ready();
    expect(c.box().value).toBe("bot2");
    expect(screen.queryByRole("button", { name: "Save login" })).toBeNull();
    fireEvent.click(c.pause());
    await waitFor(() => expect(calls).toHaveLength(2));
    expect(calls[1].body).toEqual({ on: true, paused: true, login: "bot2" });
  });

  it("shows a refusal beside the row", async () => {
    mockFetch(json(422, { error: "only https://github.com repos are in scope" }));
    await renderApp(<ProjectFactoryControls project={project} factory={{ on: false, login: "", paused: false }} />);
    fireEvent.click(screen.getByRole("switch", { name: "Factory for demo" }));
    expect((await screen.findByRole("alert")).textContent).toContain("only https://github.com repos are in scope");
  });
});
