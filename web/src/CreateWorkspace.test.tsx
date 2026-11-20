import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { useState, type ComponentProps } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { CreateWorkspace } from "./CreateWorkspace";
import { fleet } from "./fleet";
import { primaryButton } from "./Modal";
import { ME, settings, workspace } from "./test-fixtures";
import { json, mockFetch, renderApp } from "./test-utils";

afterEach(() => vi.unstubAllGlobals());

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

  it("offers only this machine for now", async () => {
    await renderApp(<Harness />);

    fireEvent.click(await screen.findByRole("button", { name: "New workspace" }));

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
      vi.fn(() => new Promise<Response>((resolve) => answers.push(resolve))),
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
