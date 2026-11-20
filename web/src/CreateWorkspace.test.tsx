import { fireEvent, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { CreateWorkspace } from "./CreateWorkspace";
import { settings } from "./test-fixtures";
import { renderApp } from "./test-utils";

function mockFetch(...answers: Response[]) {
  const calls: { url: string; method: string; body: unknown }[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init: RequestInit = {}) => {
      calls.push({ url, method: init.method ?? "GET", body: init.body ? JSON.parse(String(init.body)) : undefined });
      const next = answers.shift();
      if (!next) throw new Error("unexpected request");
      return next;
    }),
  );
  return calls;
}

const json = (status: number, body: unknown) => new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

afterEach(() => vi.unstubAllGlobals());

describe("CreateWorkspace", () => {
  it("creates the workspace, then starts a session in it, then closes", async () => {
    const calls = mockFetch(json(201, { id: "w1" }), json(201, { id: "s1" }));
    await renderApp(<CreateWorkspace projects={settings().projects} />);

    fireEvent.click(await screen.findByRole("button", { name: "New workspace" }));
    fireEvent.change(screen.getByLabelText("Project"), { target: { value: "p1" } });
    fireEvent.change(screen.getByLabelText("Name"), { target: { value: "e2e work" } });
    fireEvent.click(screen.getByRole("button", { name: "Create" }));

    await waitFor(() => expect(calls).toHaveLength(2));
    expect(calls[0]).toEqual({ url: "/api/workspaces", method: "POST", body: { project: "p1", base: "main", name: "e2e work" } });
    expect(calls[1]).toEqual({ url: "/api/workspaces/w1/sessions", method: "POST", body: { model: "opus" } });
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("offers only this machine for now", async () => {
    await renderApp(<CreateWorkspace projects={settings().projects} />);

    fireEvent.click(await screen.findByRole("button", { name: "New workspace" }));

    const machine = screen.getByLabelText("Machine") as HTMLSelectElement;
    expect(machine.value).toBe("this");
    const options = Array.from(machine.options).map((o) => [o.textContent, o.disabled]);
    expect(options).toEqual([
      ["This machine", false],
      ["Let Foundry choose (comes with placement)", true],
    ]);
  });

  it("says so and cannot submit when there are no projects", async () => {
    await renderApp(<CreateWorkspace projects={[]} />);

    fireEvent.click(await screen.findByRole("button", { name: "New workspace" }));

    expect(screen.getByText("No projects yet")).toBeDefined();
    expect((screen.getByRole("button", { name: "Create" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("keeps the dialog open and shows the error text when the workspace is refused", async () => {
    mockFetch(json(400, { error: "no such branch dev" }));
    await renderApp(<CreateWorkspace projects={settings().projects} />);

    fireEvent.click(await screen.findByRole("button", { name: "New workspace" }));
    fireEvent.change(screen.getByLabelText("Name"), { target: { value: "x" } });
    fireEvent.click(screen.getByRole("button", { name: "Create" }));

    expect((await screen.findByRole("alert")).textContent).toContain("no such branch dev");
    expect(screen.getByRole("dialog")).toBeDefined();
  });

  it("warns, without a session, when the workspace exists but its setup did not start", async () => {
    const calls = mockFetch(json(201, { id: "w1", error: "setup could not start" }));
    await renderApp(<CreateWorkspace projects={settings().projects} />);

    fireEvent.click(await screen.findByRole("button", { name: "New workspace" }));
    fireEvent.change(screen.getByLabelText("Name"), { target: { value: "x" } });
    fireEvent.click(screen.getByRole("button", { name: "Create" }));

    expect((await screen.findByRole("status")).textContent).toContain("setup could not start");
    expect(calls).toHaveLength(1);
  });
});
