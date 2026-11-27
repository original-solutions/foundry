import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import type { ComponentProps } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { taskName, useCreateWorkspace } from "./Composer";
import { Launchpad } from "./Launchpad";
import { primaryButton } from "./Modal";
import { settings } from "./test-fixtures";
import { json, mockFetch, renderApp, type Call } from "./test-utils";

afterEach(() => vi.unstubAllGlobals());

const projects = [...settings().projects, { id: "p2", name: "other", repo: "/tmp/other.git" }];

/** The launchpad with the create the layout holds for it. */
function Harness(props: Partial<ComponentProps<typeof Launchpad>>) {
  const launch = useCreateWorkspace();
  return <Launchpad projects={projects} workspaces={[]} launch={launch} onCreated={() => {}} onMenu={() => {}} {...props} />;
}

const promptBox = () => screen.getByLabelText("What should Claude work on?");
const start = () => screen.getByRole("button", { name: /^Start/ }) as HTMLButtonElement;
/** The name each request sent, if it sent one. */
const sentNames = (calls: Call[]) => calls.map((c) => (c.body as { name?: string } | undefined)?.name);

describe("taskName", () => {
  it("names a task by the first words of its first line, lower case, without closing punctuation", () => {
    expect(taskName("Fix the login redirect after sign-in.\nThen run the tests.")).toBe("fix the login redirect after");
    expect(taskName("\n\n  Tidy README!  ")).toBe("tidy readme");
    expect(taskName("   ")).toBe("");
  });

  it("keeps a name to 40 characters, cut at a word", () => {
    expect(taskName("internationalisation localisation accessibility")).toBe("internationalisation localisation");
  });

  it("names a task after the pull request or issue it points at, without a slash command's slash or other links", () => {
    expect(taskName("/review-loop https://github.com/original-solutions/people-hub/pull/1181")).toBe("review-loop pr 1181");
    expect(taskName("/review-loop https://github.com/o/r/pull/1181/files#diff-1")).toBe("review-loop pr 1181");
    expect(taskName("Fix https://github.com/o/r/issues/42 on mobile")).toBe("fix issue 42 on mobile");
    expect(taskName("review PR#1181 and fix the failing tests")).toBe("review pr 1181 and fix");
    expect(taskName("Review pr #7 then Issue #9")).toBe("review pr 7 then issue");
    expect(taskName("close issue#12")).toBe("close issue 12");
    expect(taskName("See https://example.com/docs for the API")).toBe("see for the api");
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
    await waitFor(() => expect(created).toHaveBeenCalledWith("s1", null));
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

  it("numbers a suggested name the daemon refuses as taken, makes the workspace under the first it accepts, and shows that name", async () => {
    const taken = json(409, { error: "a branch or worktree named review-loop-pr-1181 already exists in this project; give the workspace another name" });
    const calls = mockFetch(taken, json(201, { id: "w1" }), json(201, { id: "s1" }));
    const created = vi.fn();
    await renderApp(<Harness onCreated={created} />);

    fireEvent.change(promptBox(), { target: { value: "/review-loop https://github.com/original-solutions/people-hub/pull/1181" } });
    expect(screen.getByLabelText("Branch").textContent).toBe("review-loop-pr-1181");
    fireEvent.click(start());

    await waitFor(() => expect(created).toHaveBeenCalledWith("s1", null));
    expect(sentNames(calls)).toEqual(["review-loop pr 1181", "review-loop pr 1181 2", undefined]);
    expect(screen.queryByRole("alert")).toBeNull();
    expect(screen.getByLabelText("Branch").textContent).toBe("review-loop-pr-1181-2");
    expect((screen.getByLabelText("Workspace name") as HTMLInputElement).placeholder).toBe("review-loop pr 1181 2");
  });

  it("gives up on a suggested name after 20 tries and says why", async () => {
    const calls = mockFetch(...Array.from({ length: 20 }, () => json(409, { error: "a branch or worktree named fix-it already exists in this project" })));
    await renderApp(<Harness />);

    fireEvent.change(promptBox(), { target: { value: "Fix it" } });
    fireEvent.click(start());

    expect((await screen.findByRole("alert")).textContent).toMatch(/already exists in this project/);
    expect(sentNames(calls)).toEqual(["fix it", ...Array.from({ length: 19 }, (_, i) => `fix it ${i + 2}`)]);
  });

  it("does not number a typed name the daemon refuses as taken", async () => {
    const calls = mockFetch(json(409, { error: "a branch or worktree named login already exists in this project; give the workspace another name" }));
    await renderApp(<Harness />);

    fireEvent.change(promptBox(), { target: { value: "Fix it" } });
    fireEvent.change(screen.getByLabelText("Workspace name"), { target: { value: "login" } });
    fireEvent.click(start());

    expect((await screen.findByRole("alert")).textContent).toMatch(/a branch or worktree named login already exists/);
    expect(sentNames(calls)).toEqual(["login"]);
  });

  it("does not number a suggested name refused for another reason", async () => {
    const calls = mockFetch(json(400, { error: "no base branch dev" }));
    await renderApp(<Harness />);

    fireEvent.change(promptBox(), { target: { value: "Fix it" } });
    fireEvent.click(start());

    expect((await screen.findByRole("alert")).textContent).toMatch(/no base branch dev/);
    expect(sentNames(calls)).toEqual(["fix it"]);
  });

  it("says there is no project to work in when the fleet has none, and starts nothing", async () => {
    await renderApp(<Harness projects={[]} />);

    expect(screen.getByText(/No projects yet/)).toBeDefined();
    fireEvent.change(promptBox(), { target: { value: "Fix it" } });
    expect(start().disabled).toBe(true);
  });

  it("has every option on the page, with no second form behind it, and teaches the states a task shows", async () => {
    await renderApp(<Harness />);

    fireEvent.change(promptBox(), { target: { value: "Fix the login redirect." } });
    expect(screen.queryByRole("button", { name: "More options" })).toBeNull();

    const states = screen.getByRole("list", { name: "What you will see" });
    expect(within(states).getAllByRole("listitem").map((i) => i.querySelector("[data-glyph]")?.getAttribute("data-glyph"))).toEqual(["working", "waiting", "ready"]);
  });
});
