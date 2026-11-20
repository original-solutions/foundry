import { fireEvent, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { CreateWorkspace } from "./CreateWorkspace";
import { settings } from "./test-fixtures";
import { json, mockFetch, renderApp } from "./test-utils";

afterEach(() => vi.unstubAllGlobals());

const projects = [...settings().projects, { id: "p2", name: "other", repo: "/tmp/other.git" }];

async function fill(name: string) {
  fireEvent.click(await screen.findByRole("button", { name: "New workspace" }));
  fireEvent.change(screen.getByLabelText("Name"), { target: { value: name } });
}

describe("CreateWorkspace", () => {
  it("creates the workspace from what was picked, then starts a session with the model, then closes", async () => {
    const calls = mockFetch(json(201, { id: "w1" }), json(201, { id: "s1" }));
    await renderApp(<CreateWorkspace projects={projects} />);

    await fill("e2e work");
    fireEvent.change(screen.getByLabelText("Project"), { target: { value: "p2" } });
    fireEvent.change(screen.getByLabelText("Base"), { target: { value: "dev" } });
    fireEvent.change(screen.getByLabelText("Model"), { target: { value: "sonnet" } });
    fireEvent.click(screen.getByRole("button", { name: "Create" }));

    await waitFor(() => expect(calls).toHaveLength(2));
    expect(calls[0]).toEqual({ url: "/api/workspaces", method: "POST", body: { project: "p2", base: "dev", name: "e2e work" } });
    expect(calls[1]).toEqual({ url: "/api/workspaces/w1/sessions", method: "POST", body: { model: "sonnet" } });
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("offers only this machine for now", async () => {
    await renderApp(<CreateWorkspace projects={projects} />);

    fireEvent.click(await screen.findByRole("button", { name: "New workspace" }));

    const machine = screen.getByLabelText("Machine") as HTMLSelectElement;
    expect(Array.from(machine.options).map((o) => o.textContent)).toEqual(["This machine"]);
  });

  it("says so and cannot submit when there are no projects", async () => {
    await renderApp(<CreateWorkspace projects={[]} />);

    fireEvent.click(await screen.findByRole("button", { name: "New workspace" }));

    expect(screen.getByText("No projects yet")).toBeDefined();
    expect((screen.getByRole("button", { name: "Create" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("keeps the dialog open and shows the error text when the workspace is refused", async () => {
    mockFetch(json(400, { error: "no such branch dev" }));
    await renderApp(<CreateWorkspace projects={projects} />);

    await fill("x");
    fireEvent.click(screen.getByRole("button", { name: "Create" }));

    expect((await screen.findByRole("alert")).textContent).toContain("no such branch dev");
    expect(screen.getByRole("dialog")).toBeDefined();
    expect(screen.getByRole("button", { name: "Create" })).toBeDefined();
  });

  it("still starts the session when the workspace exists but its setup did not start, and shows the daemon's words once", async () => {
    const said = "workspace w1 was made, but its setup could not be started: no holder";
    const calls = mockFetch(json(201, { id: "w1", error: said }), json(201, { id: "s1" }));
    await renderApp(<CreateWorkspace projects={projects} />);

    await fill("x");
    fireEvent.change(screen.getByLabelText("Model"), { target: { value: "sonnet" } });
    fireEvent.click(screen.getByRole("button", { name: "Create" }));

    expect((await screen.findByRole("status")).textContent).toBe(said);
    expect(calls[1]).toEqual({ url: "/api/workspaces/w1/sessions", method: "POST", body: { model: "sonnet" } });
    expect(screen.queryByRole("button", { name: "Create" })).toBeNull();
    expect(screen.getByRole("button", { name: "Close" })).toBeDefined();
  });

  it("says the workspace was made when its session does not start, and offers no second create", async () => {
    mockFetch(json(201, { id: "w1" }), json(500, { error: "agent missing" }));
    await renderApp(<CreateWorkspace projects={projects} />);

    await fill("x");
    fireEvent.click(screen.getByRole("button", { name: "Create" }));

    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("The workspace was made");
    expect(alert.textContent).toContain("agent missing");
    expect(screen.queryByRole("button", { name: "Create" })).toBeNull();
    expect(screen.getByRole("button", { name: "Close" })).toBeDefined();
  });
});
