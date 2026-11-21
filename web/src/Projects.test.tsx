import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { useState, type ComponentProps } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { Projects } from "./Projects";
import { json, mockFetch, renderApp } from "./test-utils";

afterEach(() => vi.unstubAllGlobals());

/** As `/api/events` sends them: a repo's password is already hidden. */
const projects = [
  { id: "p1", name: "demo", repo: "/tmp/demo.git" },
  { id: "p2", name: "foundry", repo: "https://***@github.com/o/foundry.git" },
];

/** The dialog with a button that opens it again, as the composer holds it. */
function Closable() {
  const [open, setOpen] = useState(true);
  return (
    <>
      <button type="button" onClick={() => setOpen(true)}>
        Open projects
      </button>
      <Projects open={open} onOpenChange={setOpen} projects={projects} onAdded={() => {}} />
    </>
  );
}

function show(props: Partial<ComponentProps<typeof Projects>> = {}) {
  return renderApp(<Projects open onOpenChange={() => {}} projects={projects} onAdded={() => {}} {...props} />);
}

const dialog = () => screen.getByRole("dialog", { name: "Projects" });
const nameField = () => within(dialog()).getByLabelText("Name") as HTMLInputElement;
const repoField = () => within(dialog()).getByLabelText("Repo URL") as HTMLInputElement;
const addButton = () => within(dialog()).getByRole("button", { name: "Add" }) as HTMLButtonElement;
const row = (name: string) => within(dialog()).getByRole("listitem", { name });
const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

/** Clicks Remove on `name`'s row, then confirms it. */
function removeAndConfirm(name: string) {
  fireEvent.click(within(row(name)).getByRole("button", { name: `Remove ${name}` }));
  fireEvent.click(within(row(name)).getByRole("button", { name: "Remove from every machine" }));
}

function type(name: string, repo: string) {
  fireEvent.change(nameField(), { target: { value: name } });
  fireEvent.change(repoField(), { target: { value: repo } });
}

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
    fireEvent.change(nameField(), { target: { value: "   " } });
    expect(addButton().disabled).toBe(true);
  });

  it("posts the name and repo under a new id, then clears the form and hands over the new project's id on 201", async () => {
    const calls = mockFetch(json(201, { id: "p9" }));
    const added = vi.fn();
    await show({ onAdded: added });

    type(" api ", "git@github.com:o/api.git");
    fireEvent.click(addButton());

    await waitFor(() => expect(added).toHaveBeenCalledWith("p9"));
    expect(calls).toEqual([{ url: "/api/projects", method: "POST", body: { name: "api", repo: "git@github.com:o/api.git", id: expect.stringMatching(uuid) } }]);
    expect(nameField().value).toBe("");
    expect(repoField().value).toBe("");
  });

  it("sends an add again under the same id after an answer that came too late, and under a new one once the typing changes", async () => {
    const late = () => json(504, { error: "no answer from studio, the acting coordinator, in time" });
    const calls = mockFetch(late(), late(), json(201, { id: "p9" }), json(201, { id: "p10" }));
    await show();

    type("api", "/srv/api.git");
    fireEvent.click(addButton());
    await within(dialog()).findByRole("alert");
    fireEvent.click(addButton());
    await waitFor(() => expect(calls).toHaveLength(2));
    await waitFor(() => expect(addButton().disabled).toBe(false));
    fireEvent.change(nameField(), { target: { value: "api2" } });
    fireEvent.click(addButton());
    await waitFor(() => expect(nameField().value).toBe(""));
    type("web", "/srv/web.git");
    fireEvent.click(addButton());
    await waitFor(() => expect(calls).toHaveLength(4));

    const ids = calls.map((call) => (call.body as { id: string }).id);
    expect(ids[1]).toBe(ids[0]);
    expect(ids[2]).not.toBe(ids[1]);
    expect(ids[3]).not.toBe(ids[2]);
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

  it("clears a refused add's error when the next add goes through", async () => {
    mockFetch(json(409, { error: 'a project named "demo" already exists' }), json(201, { id: "p9" }));
    await show();

    fireEvent.change(nameField(), { target: { value: "demo" } });
    fireEvent.change(repoField(), { target: { value: "/srv/demo2.git" } });
    fireEvent.click(addButton());
    await within(dialog()).findByRole("alert");
    fireEvent.change(nameField(), { target: { value: "demo2" } });
    fireEvent.click(addButton());

    await waitFor(() => expect(nameField().value).toBe(""));
    expect(within(dialog()).queryByRole("alert")).toBeNull();
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

  it("asks once more before it removes, and sends nothing when cancelled", async () => {
    const calls = mockFetch();
    await show();

    fireEvent.click(within(row("foundry")).getByRole("button", { name: "Remove foundry" }));
    expect(within(row("foundry")).getByRole("button", { name: "Remove from every machine" })).toBeDefined();
    expect(within(row("demo")).queryByRole("button", { name: "Remove from every machine" })).toBeNull();
    fireEvent.click(within(row("foundry")).getByRole("button", { name: "Cancel" }));

    expect(within(row("foundry")).getByRole("button", { name: "Remove foundry" })).toBeDefined();
    expect(calls).toEqual([]);
  });

  it("posts a remove for that project once confirmed", async () => {
    const calls = mockFetch(new Response(null, { status: 204 }));
    await show();

    removeAndConfirm("foundry");

    await waitFor(() => expect(calls).toEqual([{ url: "/api/projects/p2/remove", method: "POST", body: undefined }]));
  });

  it("shows a 409 from Remove beside that project", async () => {
    mockFetch(json(409, { error: 'foundry still has live workspaces: "fix login" on studio; archive them first' }));
    await show();

    removeAndConfirm("foundry");

    expect((await within(row("foundry")).findByRole("alert")).textContent).toMatch(/fix login/);
    expect(within(row("demo")).queryByRole("alert")).toBeNull();
  });

  it("closing clears what was typed and every refusal", async () => {
    mockFetch(json(409, { error: 'a project named "demo" already exists' }), json(409, { error: "foundry still has live workspaces" }));
    await renderApp(<Closable />);

    type("demo", "/srv/demo2.git");
    fireEvent.click(addButton());
    await within(dialog()).findByRole("alert");
    removeAndConfirm("foundry");
    await within(row("foundry")).findByRole("alert");
    fireEvent.click(within(dialog()).getByRole("button", { name: "Close" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    fireEvent.click(screen.getByRole("button", { name: "Open projects" }));

    expect(nameField().value).toBe("");
    expect(repoField().value).toBe("");
    expect(within(dialog()).queryByRole("alert")).toBeNull();
  });
});
