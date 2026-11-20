import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { useState, type ComponentProps } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { CreateWorkspace } from "./CreateWorkspace";
import { fleet } from "./fleet";
import { bigField, field, inlineField, primaryButton } from "./Modal";
import { ME, settings, workspace } from "./test-fixtures";
import { json, renderApp, type Call } from "./test-utils";

afterEach(() => vi.unstubAllGlobals());

/** The machine list the daemon gives: this one, one online and one offline. */
const machines = {
  me: ME,
  machines: [
    { id: ME, name: "studio", online: true },
    { id: "m2", name: "laptop", online: true },
    { id: "m3", name: "old mini", online: false },
  ],
};

/**
 * Stubs `fetch` to give `answers` in order, and returns the requests it was sent. The dialog's read
 * of the machines gets `machines` and is left out, so a test sees only the requests it makes.
 */
function mockFetch(...answers: Response[]): Call[] {
  const calls: Call[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init: RequestInit = {}) => {
      if (url === "/api/machines") return json(200, machines);
      calls.push({ url, method: init.method ?? "GET", body: init.body ? JSON.parse(String(init.body)) : undefined });
      const next = answers.shift();
      if (!next) throw new Error("unexpected request");
      return next;
    }),
  );
  return calls;
}

beforeEach(() => mockFetch());

const projects = [...settings().projects, { id: "p2", name: "other", repo: "/tmp/other.git" }];
const open = fleet([workspace(ME, "w1", "fix login"), workspace(ME, "w2", "docs")], settings({ projects })).flatMap((m) => m.workspaces);

/** The dialog behind a "New workspace" button, as the layout holds it. */
function Harness(props: Partial<ComponentProps<typeof CreateWorkspace>>) {
  const [shown, setShown] = useState(false);
  return (
    <>
      <button type="button" onClick={() => setShown(true)}>
        New workspace
      </button>
      <CreateWorkspace projects={projects} workspaces={open} open={shown} onOpenChange={setShown} {...props} />
    </>
  );
}

async function fill(name: string) {
  fireEvent.click(await screen.findByRole("button", { name: "New workspace" }));
  fireEvent.change(screen.getByLabelText("Name"), { target: { value: name } });
}

describe("CreateWorkspace", () => {
  it("creates the workspace from what was picked, then starts a session with the model, then closes and opens it", async () => {
    const calls = mockFetch(json(201, { id: "w1" }), json(201, { id: "s1" }));
    const created = vi.fn();
    await renderApp(<Harness onCreated={created} />);

    await fill("e2e work");
    fireEvent.click(screen.getByRole("radio", { name: /other/ }));
    fireEvent.change(screen.getByLabelText("Base"), { target: { value: "dev" } });
    fireEvent.change(screen.getByLabelText("Model"), { target: { value: "sonnet" } });
    fireEvent.click(screen.getByRole("button", { name: "Create and open" }));

    await waitFor(() => expect(calls).toHaveLength(2));
    expect(calls[0]).toEqual({ url: "/api/workspaces", method: "POST", body: { project: "p2", base: "dev", name: "e2e work" } });
    expect(calls[1]).toEqual({ url: "/api/workspaces/w1/sessions", method: "POST", body: { model: "sonnet" } });
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(created).toHaveBeenCalledWith("s1");
  });

  it("starts the session on the first prompt written, and Cmd+Enter in it creates", async () => {
    const calls = mockFetch(json(201, { id: "w1" }), json(201, { id: "s1" }));
    await renderApp(<Harness />);

    await fill("fix the footer");
    const prompt = screen.getByLabelText("First prompt");
    fireEvent.change(prompt, { target: { value: "  Show the version in the footer.\nKeep it small.  " } });
    expect(screen.getByRole("button", { name: "Create and start" })).toBeDefined();
    fireEvent.keyDown(prompt, { key: "Enter", metaKey: true });

    await waitFor(() => expect(calls).toHaveLength(2));
    expect(calls[1]).toEqual({
      url: "/api/workspaces/w1/sessions",
      method: "POST",
      body: { model: "opus", prompt: "Show the version in the footer.\nKeep it small." },
    });
  });

  it("starts the session idle when the first prompt is blank, and Enter alone writes a new line", async () => {
    const calls = mockFetch(json(201, { id: "w1" }), json(201, { id: "s1" }));
    await renderApp(<Harness />);

    await fill("fix the footer");
    const prompt = screen.getByLabelText("First prompt");
    fireEvent.change(prompt, { target: { value: " \n " } });
    fireEvent.keyDown(prompt, { key: "Enter" });
    expect(calls).toEqual([]);
    fireEvent.click(screen.getByRole("button", { name: "Create and open" }));

    await waitFor(() => expect(calls).toHaveLength(2));
    expect(calls[1].body).toEqual({ model: "opus" });
  });

  it("shows the branch the name makes, from the base, as you type", async () => {
    await renderApp(<Harness />);

    await fill("Fix the login/redirect");
    fireEvent.change(screen.getByLabelText("Base"), { target: { value: "dev" } });

    const preview = screen.getByLabelText("Branch");
    expect(preview.textContent).toBe("Fix-the-login-redirect");
  });

  it("refuses a name whose branch the project already has, before asking the daemon", async () => {
    const calls = mockFetch();
    await renderApp(<Harness />);

    await fill("Fix Login");

    expect(screen.getByText(/demo already has a workspace on fix-login/)).toBeDefined();
    expect((screen.getByRole("button", { name: "Create and open" }) as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(screen.getByRole("radio", { name: /other/ }));
    expect((screen.getByRole("button", { name: "Create and open" }) as HTMLButtonElement).disabled).toBe(false);
    expect(calls).toEqual([]);
  });

  it("wears the shared primary button look on its one action", async () => {
    await renderApp(<Harness />);
    await fill("e2e work");
    const create = screen.getByRole("button", { name: "Create and open" });
    expect(primaryButton.split(" ").filter((c) => !create.classList.contains(c))).toEqual([]);
  });

  it("builds its Name and Base inputs from the shared field look", async () => {
    await renderApp(<Harness />);
    await fill("e2e work");
    const missing = (el: Element, look: string) => look.split(" ").filter((c) => !el.classList.contains(c));
    expect(missing(screen.getByLabelText("Name"), bigField)).toEqual([]);
    expect(missing(screen.getByLabelText("Base"), inlineField)).toEqual([]);
  });

  it("keeps a visible focus on every shared field look", () => {
    for (const look of [field, bigField, inlineField]) {
      const classes = look.split(" ");
      expect(classes).not.toContain("outline-none");
      expect(classes.some((c) => c.startsWith("focus:"))).toBe(true);
    }
  });

  it("lists each project with how many workspaces it has open, the first one picked", async () => {
    await renderApp(<Harness />);

    fireEvent.click(await screen.findByRole("button", { name: "New workspace" }));

    const group = screen.getByRole("radiogroup", { name: "Project" });
    const radios = within(group).getAllByRole("radio") as HTMLInputElement[];
    expect(radios.map((r) => r.closest("label")?.textContent)).toEqual(["demo2", "other0"]);
    expect(radios[0].checked).toBe(true);
  });

  it("starts with the name it is given, ready to create", async () => {
    await renderApp(<Harness name="fix auth" />);

    fireEvent.click(await screen.findByRole("button", { name: "New workspace" }));

    expect((screen.getByLabelText("Name") as HTMLInputElement).value).toBe("fix auth");
    expect(screen.getByLabelText("Branch").textContent).toBe("fix-auth");
  });

  it("offers this machine, letting Foundry choose, and every other listed one by name, an offline one shown but not pickable", async () => {
    await renderApp(<Harness />);

    fireEvent.click(await screen.findByRole("button", { name: "New workspace" }));

    const machine = screen.getByLabelText("Machine") as HTMLSelectElement;
    await waitFor(() => expect(machine.options).toHaveLength(4));
    expect(Array.from(machine.options).map((o) => [o.textContent, o.disabled])).toEqual([
      ["This machine", false],
      ["Let Foundry choose", false],
      ["laptop", false],
      ["old mini (offline)", true],
    ]);
    expect(machine.value).toBe("");
  });

  it("makes the workspace on the machine picked, then starts its session there", async () => {
    const calls = mockFetch(json(201, { id: "w1" }), json(201, { id: "s1" }));
    const created = vi.fn();
    await renderApp(<Harness onCreated={created} />);

    await fill("far work");
    const machine = screen.getByLabelText("Machine") as HTMLSelectElement;
    await waitFor(() => expect(machine.options).toHaveLength(4));
    fireEvent.change(machine, { target: { value: "m2" } });
    fireEvent.click(screen.getByRole("button", { name: "Create and open" }));

    await waitFor(() => expect(calls).toHaveLength(2));
    expect(calls[0]).toEqual({ url: "/api/workspaces", method: "POST", body: { project: "p1", base: "main", name: "far work", machine: "m2" } });
    expect(calls[1]).toEqual({ url: "/api/workspaces/w1/sessions", method: "POST", body: { model: "opus" } });
    await waitFor(() => expect(created).toHaveBeenCalledWith("s1"));
  });

  it("with Foundry choosing, sends the model and first prompt with the create, then starts the session where it was made", async () => {
    const calls = mockFetch(json(201, { id: "w1", machine: "m2" }), json(201, { id: "s1" }));
    const created = vi.fn();
    await renderApp(<Harness onCreated={created} />);

    await fill("anywhere");
    fireEvent.change(screen.getByLabelText("First prompt"), { target: { value: "Fix the footer." } });
    const machine = screen.getByLabelText("Machine") as HTMLSelectElement;
    await waitFor(() => expect(machine.options).toHaveLength(4));
    fireEvent.change(machine, { target: { value: "choose" } });
    fireEvent.click(screen.getByRole("button", { name: "Create and start" }));

    await waitFor(() => expect(calls).toHaveLength(2));
    expect(calls[0]).toEqual({
      url: "/api/workspaces",
      method: "POST",
      body: { project: "p1", base: "main", name: "anywhere", machine: "choose", model: "opus", prompt: "Fix the footer." },
    });
    expect(calls[1]).toEqual({ url: "/api/workspaces/w1/sessions", method: "POST", body: { model: "opus", prompt: "Fix the footer." } });
    await waitFor(() => expect(created).toHaveBeenCalledWith("s1"));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("closes on a request that waits for a machine, starts no session, and asks for the waiting list again", async () => {
    const calls = mockFetch(json(202, { request: "r1", reasons: ["laptop: load 1.8 over limit 1.0"] }));
    const created = vi.fn();
    const { client } = await renderApp(<Harness onCreated={created} />);
    const refreshed = vi.spyOn(client, "invalidateQueries");

    await fill("later");
    const machine = screen.getByLabelText("Machine") as HTMLSelectElement;
    await waitFor(() => expect(machine.options).toHaveLength(4));
    fireEvent.change(machine, { target: { value: "choose" } });
    fireEvent.click(screen.getByRole("button", { name: "Create and open" }));

    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(calls).toHaveLength(1);
    expect(calls[0].body).toEqual({ project: "p1", base: "main", name: "later", machine: "choose", model: "opus" });
    expect(created).not.toHaveBeenCalled();
    expect(refreshed).toHaveBeenCalledWith({ queryKey: ["placements"] });
  });

  it("shows the warning of a machine picked over its limits and stays open on it", async () => {
    mockFetch(json(201, { id: "w1", warning: "laptop is over its limits: load 1.5 over limit 1.0" }), json(201, { id: "s1" }));
    const created = vi.fn();
    await renderApp(<Harness onCreated={created} />);

    await fill("anyway");
    const machine = screen.getByLabelText("Machine") as HTMLSelectElement;
    await waitFor(() => expect(machine.options).toHaveLength(4));
    fireEvent.change(machine, { target: { value: "m2" } });
    fireEvent.click(screen.getByRole("button", { name: "Create and open" }));

    expect((await screen.findByRole("status")).textContent).toBe("laptop is over its limits: load 1.5 over limit 1.0");
    expect(created).toHaveBeenCalledWith("s1");
    expect(screen.getByRole("dialog")).toBeDefined();
  });

  it("still offers this machine when the machine list cannot be read", async () => {
    const read = vi.fn(async () => json(500, { error: "store unreadable" }));
    vi.stubGlobal("fetch", read);
    await renderApp(<Harness />);

    fireEvent.click(await screen.findByRole("button", { name: "New workspace" }));
    await waitFor(() => expect(read).toHaveBeenCalledWith("/api/machines"));
    await new Promise((resolve) => setTimeout(resolve, 20));

    const machine = screen.getByLabelText("Machine") as HTMLSelectElement;
    expect(Array.from(machine.options).map((o) => o.textContent)).toEqual(["This machine"]);
  });

  it("says so and cannot submit when there are no projects", async () => {
    await renderApp(<Harness projects={[]} />);

    fireEvent.click(await screen.findByRole("button", { name: "New workspace" }));

    expect(screen.getByText("No projects yet")).toBeDefined();
    expect((screen.getByRole("button", { name: "Create and open" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("keeps the dialog open and shows the error text when the workspace is refused", async () => {
    mockFetch(json(400, { error: "no such branch dev" }));
    await renderApp(<Harness />);

    await fill("x");
    fireEvent.click(screen.getByRole("button", { name: "Create and open" }));

    expect((await screen.findByRole("alert")).textContent).toContain("no such branch dev");
    expect(screen.getByRole("dialog")).toBeDefined();
    expect(screen.getByRole("button", { name: "Create and open" })).toBeDefined();
  });

  it("still starts the session when the workspace exists but its setup did not start, and shows the daemon's words once", async () => {
    const said = "workspace w1 was made, but its setup could not be started: no holder";
    const calls = mockFetch(json(201, { id: "w1", error: said }), json(201, { id: "s1" }));
    await renderApp(<Harness />);

    await fill("x");
    fireEvent.change(screen.getByLabelText("Model"), { target: { value: "sonnet" } });
    fireEvent.click(screen.getByRole("button", { name: "Create and open" }));

    expect((await screen.findByRole("status")).textContent).toBe(said);
    expect(calls[1]).toEqual({ url: "/api/workspaces/w1/sessions", method: "POST", body: { model: "sonnet" } });
    expect(screen.queryByRole("button", { name: "Create and open" })).toBeNull();
    expect(screen.getByRole("button", { name: "Close" })).toBeDefined();
  });

  it("says the workspace was made when its session does not start, and offers no second create", async () => {
    mockFetch(json(201, { id: "w1" }), json(500, { error: "agent missing" }));
    await renderApp(<Harness />);

    await fill("x");
    fireEvent.click(screen.getByRole("button", { name: "Create and open" }));

    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("The workspace was made");
    expect(alert.textContent).toContain("agent missing");
    expect(screen.queryByRole("button", { name: "Create and open" })).toBeNull();
    expect(screen.getByRole("button", { name: "Close" })).toBeDefined();
  });

  it("offers Create again when the dialog is opened after a session that did not start", async () => {
    mockFetch(json(201, { id: "w1" }), json(500, { error: "agent missing" }));
    await renderApp(<Harness />);

    await fill("x");
    fireEvent.click(screen.getByRole("button", { name: "Create and open" }));
    await screen.findByRole("alert");
    fireEvent.click(screen.getByRole("button", { name: "Close" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());

    fireEvent.click(screen.getByRole("button", { name: "New workspace" }));
    expect(screen.getByRole("button", { name: "Create and open" })).toBeDefined();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("is a fresh dialog, left open, when an earlier create cancelled on its way finishes", async () => {
    const answers: ((response: Response) => void)[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => (url === "/api/machines" ? Promise.resolve(json(200, machines)) : new Promise<Response>((resolve) => answers.push(resolve)))),
    );
    await renderApp(<Harness />);

    await fill("x");
    fireEvent.click(screen.getByRole("button", { name: "Create and open" }));
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    fireEvent.click(screen.getByRole("button", { name: "New workspace" }));
    answers[0](json(201, { id: "w1" }));
    await waitFor(() => expect(answers).toHaveLength(2));
    answers[1](json(201, { id: "s1" }));
    await new Promise((resolve) => setTimeout(resolve, 20));

    expect(screen.getByRole("dialog")).toBeDefined();
    expect(screen.getByRole("button", { name: "Create and open" })).toBeDefined();
    expect(screen.getByRole("button", { name: "Cancel" })).toBeDefined();
  });
});
