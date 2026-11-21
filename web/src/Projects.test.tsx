import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import type { ComponentProps } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { Projects } from "./Projects";
import { json, mockFetch, renderApp } from "./test-utils";

afterEach(() => vi.unstubAllGlobals());

/** As `/api/events` sends them: a repo's password is already hidden. */
const projects = [
  { id: "p1", name: "demo", repo: "/tmp/demo.git" },
  { id: "p2", name: "foundry", repo: "https://***@github.com/o/foundry.git" },
];

function show(props: Partial<ComponentProps<typeof Projects>> = {}) {
  return renderApp(<Projects open onOpenChange={() => {}} projects={projects} onAdded={() => {}} {...props} />);
}

const dialog = () => screen.getByRole("dialog", { name: "Projects" });
const nameField = () => within(dialog()).getByLabelText("Name") as HTMLInputElement;
const repoField = () => within(dialog()).getByLabelText("Repo URL") as HTMLInputElement;
const addButton = () => within(dialog()).getByRole("button", { name: "Add" }) as HTMLButtonElement;
const row = (name: string) => within(dialog()).getByRole("listitem", { name });

describe("Projects", () => {
  it("lists each project's name and its repo as the settings show it, each with a Remove button", async () => {
    await show();

    expect(within(row("demo")).getByText("/tmp/demo.git")).toBeDefined();
    expect(within(row("foundry")).getByText("https://***@github.com/o/foundry.git")).toBeDefined();
    expect(within(row("demo")).getByRole("button", { name: "Remove demo" })).toBeDefined();
    expect(within(row("foundry")).getByRole("button", { name: "Remove foundry" })).toBeDefined();
  });

  it("says so when there are no projects", async () => {
    await show({ projects: [] });

    expect(within(dialog()).queryByRole("list")).toBeNull();
    expect(within(dialog()).getByText(/no projects yet/i)).toBeDefined();
  });

  it("keeps Add disabled until both a name and a repo URL are typed", async () => {
    await show();

    expect(addButton().disabled).toBe(true);
    fireEvent.change(nameField(), { target: { value: "api" } });
    expect(addButton().disabled).toBe(true);
    fireEvent.change(repoField(), { target: { value: "   " } });
    expect(addButton().disabled).toBe(true);
    fireEvent.change(repoField(), { target: { value: "git@github.com:o/api.git" } });
    expect(addButton().disabled).toBe(false);
  });

  it("posts the name and repo, then clears the form and hands over the new project's id on 201", async () => {
    const calls = mockFetch(json(201, { id: "p9" }));
    const added = vi.fn();
    await show({ onAdded: added });

    fireEvent.change(nameField(), { target: { value: " api " } });
    fireEvent.change(repoField(), { target: { value: "git@github.com:o/api.git" } });
    fireEvent.click(addButton());

    await waitFor(() => expect(added).toHaveBeenCalledWith("p9"));
    expect(calls).toEqual([{ url: "/api/projects", method: "POST", body: { name: "api", repo: "git@github.com:o/api.git" } }]);
    expect(nameField().value).toBe("");
    expect(repoField().value).toBe("");
  });

  it("keeps Add disabled while the request is pending", async () => {
    let answer: (response: Response) => void = () => {};
    vi.stubGlobal("fetch", vi.fn(() => new Promise<Response>((resolve) => (answer = resolve))));
    await show();

    fireEvent.change(nameField(), { target: { value: "api" } });
    fireEvent.change(repoField(), { target: { value: "/srv/api.git" } });
    fireEvent.click(addButton());

    await waitFor(() => expect(addButton().disabled).toBe(true));
    answer(json(201, { id: "p9" }));
    await waitFor(() => expect(nameField().value).toBe(""));
  });

  it("shows a 409's error in the form and keeps what was typed", async () => {
    mockFetch(json(409, { error: 'a project named "demo" already exists' }));
    const added = vi.fn();
    await show({ onAdded: added });

    fireEvent.change(nameField(), { target: { value: "Demo" } });
    fireEvent.change(repoField(), { target: { value: "/srv/demo2.git" } });
    fireEvent.click(addButton());

    expect((await within(dialog()).findByRole("alert")).textContent).toBe('a project named "demo" already exists');
    expect(nameField().value).toBe("Demo");
    expect(repoField().value).toBe("/srv/demo2.git");
    expect(added).not.toHaveBeenCalled();
  });

  it("shows a 422's error in the form and keeps what was typed", async () => {
    mockFetch(json(422, { error: "a repo URL holds no spaces, tabs, line breaks or NULs" }));
    await show();

    fireEvent.change(nameField(), { target: { value: "api" } });
    fireEvent.change(repoField(), { target: { value: "/srv/my repo.git" } });
    fireEvent.click(addButton());

    expect((await within(dialog()).findByRole("alert")).textContent).toMatch(/no spaces/);
    expect(repoField().value).toBe("/srv/my repo.git");
  });

  it("posts a remove for that project", async () => {
    const calls = mockFetch(new Response(null, { status: 204 }));
    await show();

    fireEvent.click(within(row("foundry")).getByRole("button", { name: "Remove foundry" }));

    await waitFor(() => expect(calls).toEqual([{ url: "/api/projects/p2/remove", method: "POST", body: undefined }]));
  });

  it("shows a 409 from Remove beside that project", async () => {
    mockFetch(json(409, { error: 'foundry still has live workspaces: "fix login" on studio; archive them first' }));
    await show();

    fireEvent.click(within(row("foundry")).getByRole("button", { name: "Remove foundry" }));

    expect((await within(row("foundry")).findByRole("alert")).textContent).toMatch(/fix login/);
    expect(within(row("demo")).queryByRole("alert")).toBeNull();
  });
});
