import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import type { ComponentProps } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useCreateWorkspace } from "./CreateWorkspace";
import { Launchpad, taskName } from "./Launchpad";
import { primaryButton } from "./Modal";
import { settings } from "./test-fixtures";
import { json, mockFetch, renderApp } from "./test-utils";

afterEach(() => vi.unstubAllGlobals());

const projects = [...settings().projects, { id: "p2", name: "other", repo: "/tmp/other.git" }];

/** The launchpad with the create the layout holds for it. */
function Harness(props: Partial<ComponentProps<typeof Launchpad>>) {
  const launch = useCreateWorkspace();
  return <Launchpad projects={projects} launch={launch} onCreated={() => {}} onMore={() => {}} onMenu={() => {}} {...props} />;
}

const promptBox = () => screen.getByLabelText("What should Claude work on?");
const start = () => screen.getByRole("button", { name: /^Start/ }) as HTMLButtonElement;

describe("taskName", () => {
  it("names a task by the first words of its first line, lower case, without closing punctuation", () => {
    expect(taskName("Fix the login redirect after sign-in.\nThen run the tests.")).toBe("fix the login redirect after");
    expect(taskName("\n\n  Tidy README!  ")).toBe("tidy readme");
    expect(taskName("   ")).toBe("");
  });

  it("keeps a name to 40 characters, cut at a word", () => {
    expect(taskName("internationalisation localisation accessibility")).toBe("internationalisation localisation");
  });
});

describe("Launchpad", () => {
  it("asks what Claude should work on, and names the workspace and its branch from the task as it is typed", async () => {
    await renderApp(<Harness />);

    expect(screen.getByRole("heading", { name: "What should Claude work on?" })).toBeDefined();
    fireEvent.change(promptBox(), { target: { value: "Fix the login redirect.\nKeep it small." } });

    expect(screen.getByLabelText("Branch").textContent).toBe("fix-the-login-redirect");
    expect((screen.getByLabelText("Workspace name") as HTMLInputElement).placeholder).toBe("fix the login redirect");
  });

  it("makes the workspace in the picked project and starts Claude on the task, then hands over the session", async () => {
    const calls = mockFetch(json(201, { id: "w1" }), json(201, { id: "s1" }));
    const created = vi.fn();
    await renderApp(<Harness onCreated={created} />);

    fireEvent.change(promptBox(), { target: { value: "  Fix the login redirect.\nKeep it small. " } });
    fireEvent.click(screen.getByRole("radio", { name: "other" }));
    fireEvent.keyDown(promptBox(), { key: "Enter", metaKey: true });

    await waitFor(() => expect(calls).toHaveLength(2));
    expect(calls[0]).toEqual({ url: "/api/workspaces", method: "POST", body: { project: "p2", base: "main", name: "fix the login redirect" } });
    expect(calls[1]).toEqual({ url: "/api/workspaces/w1/sessions", method: "POST", body: { model: "opus", prompt: "Fix the login redirect.\nKeep it small." } });
    await waitFor(() => expect(created).toHaveBeenCalledWith("s1"));
  });

  it("takes a typed name, base and model over what it would pick", async () => {
    const calls = mockFetch(json(201, { id: "w1" }), json(201, { id: "s1" }));
    await renderApp(<Harness />);

    fireEvent.change(promptBox(), { target: { value: "Fix the login redirect." } });
    fireEvent.change(screen.getByLabelText("Workspace name"), { target: { value: "login" } });
    fireEvent.change(screen.getByLabelText("Base"), { target: { value: "dev" } });
    fireEvent.change(screen.getByLabelText("Model"), { target: { value: "sonnet" } });
    expect(screen.getByLabelText("Branch").textContent).toBe("login");
    fireEvent.click(start());

    await waitFor(() => expect(calls).toHaveLength(2));
    expect(calls[0].body).toEqual({ project: "p1", base: "dev", name: "login" });
    expect(calls[1].body).toEqual({ model: "sonnet", prompt: "Fix the login redirect." });
  });

  it("starts nothing until there is a task or a name, and Enter alone writes a new line", async () => {
    const calls = mockFetch();
    await renderApp(<Harness />);

    expect(start().disabled).toBe(true);
    fireEvent.keyDown(promptBox(), { key: "Enter", metaKey: true });
    fireEvent.change(promptBox(), { target: { value: "Fix it" } });
    fireEvent.keyDown(promptBox(), { key: "Enter" });
    expect(start().disabled).toBe(false);
    expect(calls).toEqual([]);
    expect(primaryButton.split(" ").filter((c) => !start().classList.contains(c))).toEqual([]);
  });

  it("says what it is doing while it works", async () => {
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));
    await renderApp(<Harness />);

    fireEvent.change(promptBox(), { target: { value: "Fix it" } });
    fireEvent.click(start());

    expect(await screen.findByText("Making fix-it from main…")).toBeDefined();
    expect(start().disabled).toBe(true);
  });

  it("says what went wrong when the session does not start, and lets the board show the workspace it made", async () => {
    mockFetch(json(201, { id: "w1" }), json(500, { error: "claude not found" }));
    await renderApp(<Harness />);

    fireEvent.change(promptBox(), { target: { value: "Fix it" } });
    fireEvent.click(start());

    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toMatch(/The workspace was made, but its session did not start: claude not found/);
    expect(start().disabled).toBe(true);
    fireEvent.click(within(alert).getByRole("button", { name: "Go to the board" }));
    await waitFor(() => expect(screen.queryByRole("alert")).toBeNull());
  });

  it("says there is no project to work in when the fleet has none, and starts nothing", async () => {
    await renderApp(<Harness projects={[]} />);

    expect(screen.getByText(/No projects yet/)).toBeDefined();
    fireEvent.change(promptBox(), { target: { value: "Fix it" } });
    expect(start().disabled).toBe(true);
  });

  it("opens the full dialog with the name it would use, and teaches the states a task shows", async () => {
    const more = vi.fn();
    await renderApp(<Harness onMore={more} />);

    fireEvent.change(promptBox(), { target: { value: "Fix the login redirect." } });
    fireEvent.click(screen.getByRole("button", { name: "More options" }));
    expect(more).toHaveBeenCalledWith("fix the login redirect");

    const states = screen.getByRole("list", { name: "What you will see" });
    expect(within(states).getAllByRole("listitem").map((i) => i.querySelector("[data-glyph]")?.getAttribute("data-glyph"))).toEqual(["working", "waiting", "ready"]);
  });
});
