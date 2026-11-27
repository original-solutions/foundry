import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { useState, type ComponentProps } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { fleet } from "./fleet";
import { Composer, useCreateWorkspace } from "./Composer";
import { NewTask } from "./NewTask";
import { ME, session, settings, terminal, workspace } from "./test-fixtures";
import { json, mockFetch, renderApp } from "./test-utils";

afterEach(() => vi.unstubAllGlobals());

const projects = settings().projects;
const open = fleet(
  [workspace(ME, "w1", "fix login"), terminal(ME, "t1", "w1", "running"), session(ME, "s1", "w1", "t1", "idle")],
  settings({ projects }),
).flatMap((m) => m.workspaces);

/** The dialog behind a "New task" button, as the layout holds it. */
function Harness(props: Partial<ComponentProps<typeof NewTask>>) {
  const [shown, setShown] = useState(false);
  return (
    <>
      <button type="button" onClick={() => setShown(true)}>
        New task
      </button>
      <NewTask projects={projects} workspaces={open} open={shown} onOpenChange={setShown} onCreated={() => {}} {...props} />
    </>
  );
}

const show = async () => {
  fireEvent.click(await screen.findByRole("button", { name: "New task" }));
  return screen.getByRole("dialog", { name: "New task" });
};
const taskBox = (dialog: HTMLElement) => within(dialog).getByLabelText("What should Claude work on?") as HTMLTextAreaElement;
const list = "- Fix the footer\n  It overlaps the cart.\n- Tidy the README\n- Add CSV export";

describe("a task written as a list", () => {
  it("offers to start each item as its own workspace only when the task is a list", async () => {
    await renderApp(<Harness />);
    const dialog = await show();

    fireEvent.change(taskBox(dialog), { target: { value: "Fix the footer." } });
    expect(within(dialog).queryByRole("button", { name: /^Split into/ })).toBeNull();

    fireEvent.change(taskBox(dialog), { target: { value: list } });
    const split = within(dialog).getByRole("button", { name: "Split into 3 tasks" });
    expect(split.getAttribute("aria-pressed")).toBe("false");
    expect(within(dialog).getByRole("button", { name: "Start" })).toBeDefined();
  });

  it("split, shows the workspace and branch each item makes, and Start makes them one after another, each on its item, without landing", async () => {
    const calls = mockFetch(json(201, { id: "w7" }), json(201, { id: "s7" }), json(201, { id: "w8" }), json(201, { id: "s8" }), json(201, { id: "w9" }), json(201, { id: "s9" }));
    const created = vi.fn();
    await renderApp(<Harness onCreated={created} />);
    const dialog = await show();

    fireEvent.change(taskBox(dialog), { target: { value: list } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Split into 3 tasks" }));

    expect(within(dialog).getByRole("button", { name: "Split into 3 tasks" }).getAttribute("aria-pressed")).toBe("true");
    const planned = within(dialog).getByRole("list", { name: "Workspaces to make" });
    expect(within(planned).getAllByLabelText("Branch").map((b) => b.textContent)).toEqual(["fix-the-footer", "tidy-the-readme", "add-csv-export"]);
    expect(within(dialog).queryByLabelText("Workspace name")).toBeNull();

    fireEvent.click(within(dialog).getByRole("button", { name: "Start 3" }));

    await waitFor(() => expect(calls).toHaveLength(6));
    expect(calls.map((c) => c.url)).toEqual(["/api/workspaces", "/api/workspaces/w7/sessions", "/api/workspaces", "/api/workspaces/w8/sessions", "/api/workspaces", "/api/workspaces/w9/sessions"]);
    expect(calls[0].body).toEqual({ project: "p1", base: "main", name: "fix the footer" });
    expect(calls[1].body).toEqual({ model: "opus", prompt: "Fix the footer\nIt overlaps the cart." });
    expect(calls[2].body).toEqual({ project: "p1", base: "main", name: "tidy the readme" });
    expect(calls[5].body).toEqual({ model: "opus", prompt: "Add CSV export" });
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(created).not.toHaveBeenCalled();
  });

  it("numbers names a project already has, and names the items share", async () => {
    await renderApp(<Harness />);
    const dialog = await show();

    fireEvent.change(taskBox(dialog), { target: { value: "- Fix login\n- Fix login" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Split into 2 tasks" }));

    const planned = within(dialog).getByRole("list", { name: "Workspaces to make" });
    expect(within(planned).getAllByLabelText("Branch").map((b) => b.textContent)).toEqual(["fix-login-2", "fix-login-3"]);
  });

  it("unsplit, Start makes one workspace with the whole list as its task, named from its first item", async () => {
    const calls = mockFetch(json(201, { id: "w9" }), json(201, { id: "s9" }));
    await renderApp(<Harness />);
    const dialog = await show();

    fireEvent.change(taskBox(dialog), { target: { value: "- Fix the footer\n- Tidy the README" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Start" }));

    await waitFor(() => expect(calls).toHaveLength(2));
    expect(calls[0].body).toEqual({ project: "p1", base: "main", name: "fix the footer" });
    expect(calls[1].body).toEqual({ model: "opus", prompt: "- Fix the footer\n- Tidy the README" });
  });

  it("keeps the items that did not start in the box, says why, and stays open", async () => {
    const calls = mockFetch(json(201, { id: "w7" }), json(201, { id: "s7" }), json(409, { error: "the branch tidy-the-readme exists" }), json(201, { id: "w9" }), json(201, { id: "s9" }));
    await renderApp(<Harness />);
    const dialog = await show();

    fireEvent.change(taskBox(dialog), { target: { value: list } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Split into 3 tasks" }));
    fireEvent.keyDown(taskBox(dialog), { key: "Enter", metaKey: true });

    await waitFor(() => expect(calls).toHaveLength(5));
    const alert = await within(dialog).findByRole("alert");
    expect(alert.textContent).toContain("Started 2 of 3.");
    expect(alert.textContent).toContain("tidy the readme: the branch tidy-the-readme exists");
    expect(taskBox(dialog).value).toBe("Tidy the README");
    expect(screen.getByRole("dialog", { name: "New task" })).toBeDefined();
  });

  it("drops an item whose workspace was made from the box even when its session did not start", async () => {
    mockFetch(json(201, { id: "w7" }), json(500, { error: "no claude" }), json(201, { id: "w8" }), json(201, { id: "s8" }), json(409, { error: "taken" }));
    await renderApp(<Harness />);
    const dialog = await show();

    fireEvent.change(taskBox(dialog), { target: { value: list } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Split into 3 tasks" }));
    fireEvent.click(within(dialog).getByRole("button", { name: "Start 3" }));

    const alert = await within(dialog).findByRole("alert");
    expect(alert.textContent).toContain("Started 1 of 3.");
    expect(alert.textContent).toContain("fix the footer: The workspace was made, but its session did not start: no claude.");
    expect(taskBox(dialog).value).toBe("Add CSV export");
  });

  it("on the board's line, empties the box once every item started, ready for the next task", async () => {
    const calls = mockFetch(json(201, { id: "w7" }), json(201, { id: "s7" }), json(201, { id: "w8" }), json(201, { id: "s8" }));
    function Bar() {
      const launch = useCreateWorkspace();
      return <Composer mode="bar" id="board-task" projects={projects} workspaces={open} launch={launch} onCreated={() => {}} doneLabel="Clear" />;
    }
    await renderApp(<Bar />);
    const box = screen.getByLabelText("New task") as HTMLTextAreaElement;

    fireEvent.change(box, { target: { value: "1. Fix the footer\n2. Tidy the README" } });
    fireEvent.click(screen.getByRole("button", { name: "Split into 2 tasks" }));
    fireEvent.click(screen.getByRole("button", { name: "Start 2" }));

    await waitFor(() => expect(box.value).toBe(""));
    expect(calls).toHaveLength(4);
    expect(screen.queryByRole("list", { name: "Workspaces to make" })).toBeNull();
  });
});
