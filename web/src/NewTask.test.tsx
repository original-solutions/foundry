import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { useState, type ComponentProps } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { fleet } from "./fleet";
import { field, fieldLook, inlineField } from "./Modal";
import { NewTask } from "./NewTask";
import { ME, session, settings, terminal, workspace } from "./test-fixtures";
import { json, mockFetch, renderApp, type Call } from "./test-utils";

afterEach(() => vi.unstubAllGlobals());

const projects = [...settings().projects, { id: "p2", name: "other", repo: "/tmp/other.git" }];
const open = fleet(
  [workspace(ME, "w1", "fix login"), terminal(ME, "t1", "w1", "running"), session(ME, "s1", "w1", "t1", "idle", { model: "haiku-test" })],
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

describe("NewTask", () => {
  it("is the task box, open: the task names the workspace, Cmd+Enter makes it and starts Claude on it, then closes and hands over the session", async () => {
    const calls = mockFetch(json(201, { id: "w9" }), json(201, { id: "s9" }));
    const created = vi.fn();
    await renderApp(<Harness onCreated={created} />);

    const dialog = await show();
    expect(document.activeElement).toBe(taskBox(dialog));
    fireEvent.change(taskBox(dialog), { target: { value: "Add search to the board." } });
    expect(within(dialog).getByLabelText("Branch").textContent).toBe("add-search-to-the-board");
    fireEvent.click(within(dialog).getByRole("radio", { name: "other" }));
    fireEvent.keyDown(taskBox(dialog), { key: "Enter", metaKey: true });

    await waitFor(() => expect(calls).toHaveLength(2));
    expect(calls[0]).toEqual({ url: "/api/workspaces", method: "POST", body: { project: "p2", base: "main", name: "add search to the board" } });
    expect(calls[1]).toEqual({ url: "/api/workspaces/w9/sessions", method: "POST", body: { model: "opus", prompt: "Add search to the board." } });
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(created).toHaveBeenCalledWith("s9");
  });

  it("opens on the task it is given, as typed in the jump palette", async () => {
    await renderApp(<Harness task="fix auth" />);

    const dialog = await show();
    expect(taskBox(dialog).value).toBe("fix auth");
    expect(within(dialog).getByLabelText("Branch").textContent).toBe("fix-auth");
  });

  it("makes a workspace with Claude idle in it when no task is written and a name is", async () => {
    const calls = mockFetch(json(201, { id: "w9" }), json(201, { id: "s9" }));
    await renderApp(<Harness />);

    const dialog = await show();
    const start = within(dialog).getByRole("button", { name: "Start" }) as HTMLButtonElement;
    expect(start.disabled).toBe(true);
    fireEvent.change(within(dialog).getByLabelText("Workspace name"), { target: { value: "e2e work" } });
    fireEvent.change(within(dialog).getByLabelText("Model"), { target: { value: "sonnet" } });
    fireEvent.click(start);

    await waitFor(() => expect(calls).toHaveLength(2));
    expect(calls[0].body).toEqual({ project: "p1", base: "main", name: "e2e work" });
    expect(calls[1].body).toEqual({ model: "sonnet" });
  });

  it("offers the models the fleet already runs beside opus and sonnet", async () => {
    await renderApp(<Harness />);

    const dialog = await show();
    const list = dialog.querySelector(`#${within(dialog).getByLabelText("Model").getAttribute("list")}`)!;
    expect(Array.from(list.querySelectorAll("option")).map((o) => o.getAttribute("value"))).toEqual(["opus", "sonnet", "haiku-test"]);
  });

  it("names no machine picker it cannot honour, and says where the task runs", async () => {
    await renderApp(<Harness />);

    const dialog = await show();
    expect(within(dialog).queryByLabelText("Machine")).toBeNull();
    expect(within(dialog).getByText(/on this machine/)).toBeDefined();
  });

  it("offers each other machine by name, an offline one shown but not pickable, and makes the workspace on the one picked", async () => {
    const machines = {
      me: ME,
      machines: [
        { id: ME, name: "studio", online: true },
        { id: "m2", name: "laptop", online: true },
        { id: "m3", name: "old mini", online: false },
      ],
    };
    const calls: Call[] = [];
    const answers = [json(201, { id: "w9" }), json(201, { id: "s9" })];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init: RequestInit = {}) => {
        if (url === "/api/machines") return json(200, machines);
        calls.push({ url, method: init.method ?? "GET", body: init.body ? JSON.parse(String(init.body)) : undefined });
        return answers.shift() ?? json(500, { error: "unexpected request" });
      }),
    );
    await renderApp(<Harness />);

    const dialog = await show();
    const machine = (await within(dialog).findByLabelText("Machine")) as HTMLSelectElement;
    expect(Array.from(machine.options).map((o) => [o.textContent, o.disabled])).toEqual([
      ["this machine", false],
      ["laptop", false],
      ["old mini (offline)", true],
    ]);
    expect(machine.value).toBe("");
    expect(within(dialog).queryByText(/on this machine/)).toBeNull();

    fireEvent.change(taskBox(dialog), { target: { value: "Far work" } });
    fireEvent.change(machine, { target: { value: "m2" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Start" }));

    await waitFor(() => expect(calls).toHaveLength(2));
    expect(calls[0].body).toEqual({ project: "p1", base: "main", name: "far work", machine: "m2" });
    expect(calls[1]).toEqual({ url: "/api/workspaces/w9/sessions", method: "POST", body: { model: "opus", prompt: "Far work" } });
  });

  it("still makes the workspace on this machine when the machine list cannot be read", async () => {
    const read = vi.fn(async () => json(500, { error: "store unreadable" }));
    vi.stubGlobal("fetch", read);
    await renderApp(<Harness />);

    const dialog = await show();
    await waitFor(() => expect(read).toHaveBeenCalledWith("/api/machines"));
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(within(dialog).queryByLabelText("Machine")).toBeNull();
    expect(within(dialog).getByText(/on this machine/)).toBeDefined();
  });

  it("stays open with the daemon's words when the workspace is refused", async () => {
    mockFetch(json(400, { error: "no such branch dev" }));
    await renderApp(<Harness />);

    const dialog = await show();
    fireEvent.change(taskBox(dialog), { target: { value: "x" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Start" }));

    expect((await within(dialog).findByRole("alert")).textContent).toContain("no such branch dev");
    expect(screen.getByRole("dialog")).toBeDefined();
  });

  it("stays open with the daemon's words when the workspace was made but its setup did not start, and still hands over the session", async () => {
    const said = "workspace w9 was made, but its setup could not be started: no holder";
    mockFetch(json(201, { id: "w9", error: said }), json(201, { id: "s9" }));
    const created = vi.fn();
    await renderApp(<Harness onCreated={created} />);

    const dialog = await show();
    fireEvent.change(taskBox(dialog), { target: { value: "x" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Start" }));

    expect((await within(dialog).findByRole("status")).textContent).toBe(`${said}Close`);
    expect(created).toHaveBeenCalledWith("s9");
    fireEvent.click(within(dialog).getByRole("button", { name: "Close" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("says the workspace was made when its session does not start, offers Close, and opens fresh after", async () => {
    mockFetch(json(201, { id: "w9" }), json(500, { error: "agent missing" }));
    await renderApp(<Harness />);

    let dialog = await show();
    fireEvent.change(taskBox(dialog), { target: { value: "x" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Start" }));
    const alert = await within(dialog).findByRole("alert");
    expect(alert.textContent).toContain("The workspace was made");
    expect((within(dialog).getByRole("button", { name: "Start" }) as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(within(dialog).getByRole("button", { name: "Close" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());

    dialog = await show();
    expect(taskBox(dialog).value).toBe("");
    expect(within(dialog).queryByRole("alert")).toBeNull();
  });

  it("keeps a visible focus on every shared field look", () => {
    for (const look of [field, inlineField, fieldLook]) {
      const classes = look.split(" ");
      expect(classes).not.toContain("outline-none");
      expect(classes.some((c) => c.startsWith("focus:"))).toBe(true);
    }
  });
});
