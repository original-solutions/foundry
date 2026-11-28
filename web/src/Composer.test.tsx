import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Project } from "./api";
import { Composer, useCreateWorkspace } from "./Composer";
import { json, mockFetch, renderApp } from "./test-utils";

afterEach(() => vi.unstubAllGlobals());

const demo = { id: "p1", name: "demo", repo: "/tmp/demo.git" };
const api = { id: "p9", name: "api", repo: "git@github.com:o/api.git" };

/**
 * The task box as a page holds it, with the projects the settings hold. "Settings arrive" stands in
 * for `/api/events` sending settings that hold `api` too.
 */
function Harness({ initial }: { initial: Project[] }) {
  const launch = useCreateWorkspace();
  const [projects, setProjects] = useState(initial);
  return (
    <>
      <button type="button" onClick={() => setProjects([...initial, api])}>
        Settings arrive
      </button>
      <Composer mode="page" id="task" projects={projects} workspaces={[]} launch={launch} onCreated={() => {}} doneLabel="Close" />
    </>
  );
}

describe("Composer projects", () => {
  it("shows each project with its face, the one its workspaces carry everywhere", async () => {
    await renderApp(<Harness initial={[demo, api]} />);
    const radio = screen.getByRole("radio", { name: "api" });
    expect(radio.closest("label")?.querySelector("[title='api']")?.textContent).toBe("AP");
  });

  it("offers to add a project when there are none, and that opens the Projects dialog", async () => {
    await renderApp(<Harness initial={[]} />);

    expect(screen.getByText(/No projects yet/)).toBeDefined();
    fireEvent.click(screen.getByRole("button", { name: "Add a project" }));

    const dialog = await screen.findByRole("dialog", { name: "Projects" });
    expect(within(dialog).getByLabelText("Name")).toBeDefined();
  });

  it("shows the Projects control in the options row with no projects, and after the project radios when there are some", async () => {
    const { unmount } = await renderApp(<Harness initial={[]} />);
    expect(screen.getByRole("button", { name: "Projects" })).toBeDefined();
    unmount();

    await renderApp(<Harness initial={[demo]} />);
    const radios = screen.getByRole("radiogroup", { name: "Project" });
    const control = screen.getByRole("button", { name: "Projects" });
    expect(radios.compareDocumentPosition(control) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    fireEvent.click(control);
    expect(await screen.findByRole("dialog", { name: "Projects" })).toBeDefined();
  });

  it("selects a project once it is added and arrives in the settings", async () => {
    mockFetch(json(201, { id: "p9" }));
    await renderApp(<Harness initial={[demo]} />);
    expect((screen.getByRole("radio", { name: "demo" }) as HTMLInputElement).checked).toBe(true);

    fireEvent.click(screen.getByRole("button", { name: "Projects" }));
    const dialog = await screen.findByRole("dialog", { name: "Projects" });
    fireEvent.change(within(dialog).getByLabelText("Name"), { target: { value: "api" } });
    fireEvent.change(within(dialog).getByLabelText("Repo URL"), { target: { value: "git@github.com:o/api.git" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Add" }));
    await waitFor(() => expect((within(dialog).getByLabelText("Name") as HTMLInputElement).value).toBe(""));

    fireEvent.keyDown(dialog, { key: "Escape" });
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    fireEvent.click(screen.getByRole("button", { name: "Settings arrive" }));

    expect((screen.getByRole("radio", { name: "api" }) as HTMLInputElement).checked).toBe(true);
    expect((screen.getByRole("radio", { name: "demo" }) as HTMLInputElement).checked).toBe(false);
  });

  it("does not start a workspace when a project is added", async () => {
    const calls = mockFetch(json(201, { id: "p9" }));
    await renderApp(<Harness initial={[demo]} />);
    fireEvent.change(document.getElementById("task") as HTMLTextAreaElement, { target: { value: "Fix it" } });

    fireEvent.click(screen.getByRole("button", { name: "Projects" }));
    const dialog = await screen.findByRole("dialog", { name: "Projects" });
    fireEvent.change(within(dialog).getByLabelText("Name"), { target: { value: "api" } });
    fireEvent.change(within(dialog).getByLabelText("Repo URL"), { target: { value: "/srv/api.git" } });
    fireEvent.submit(within(dialog).getByLabelText("Name"));

    await waitFor(() => expect((within(dialog).getByLabelText("Name") as HTMLInputElement).value).toBe(""));
    // A workspace create would have been sent by now: it starts from the same submit.
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(calls.map((c) => c.url)).toEqual(["/api/projects"]);
    expect(screen.queryByText(/^Making /)).toBeNull();
  });

  it("keeps the bar's options row open while the Projects dialog is up", async () => {
    function Bar() {
      const launch = useCreateWorkspace();
      return <Composer mode="bar" id="bar" projects={[demo]} workspaces={[]} launch={launch} onCreated={() => {}} doneLabel="Clear" />;
    }
    await renderApp(<Bar />);
    fireEvent.focus(document.getElementById("bar") as HTMLTextAreaElement);
    fireEvent.click(screen.getByRole("button", { name: "Projects" }));
    const dialog = await screen.findByRole("dialog", { name: "Projects" });

    fireEvent.focus(within(dialog).getByLabelText("Name"));
    fireEvent.blur(document.getElementById("bar") as HTMLTextAreaElement, { relatedTarget: within(dialog).getByLabelText("Name") });

    expect(screen.getByRole("button", { name: "Projects", hidden: true })).toBeDefined();
  });
});
