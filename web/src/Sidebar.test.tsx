import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { fleet } from "./fleet";
import { Sidebar } from "./Sidebar";
import { ME, OTHER, sample, session, settings, terminal, workspace } from "./test-fixtures";
import { json, mockFetch, renderApp } from "./test-utils";
import type { SessionState } from "./api";

const states: [SessionState, string][] = [
  ["starting", "Starting"],
  ["working", "Working"],
  ["waiting_for_you", "Waiting for you"],
  ["idle", "Idle"],
  ["finished", "Finished"],
  ["failed", "Failed"],
  ["stuck", "Stuck"],
  ["unreachable", "Unreachable"],
];

function machines() {
  const records = [
    sample(ME, 1),
    workspace(ME, "w1", "fix login"),
    ...states.flatMap(([state], i) => [
      terminal(ME, `st${i}`, "w1", "running"),
      session(ME, `s${i}`, "w1", `st${i}`, state, { model: `model-${i}` }),
    ]),
    terminal(ME, "sh1", "w1", "running"),
    terminal(ME, "sh2", "w1", "exited", { exit_code: 1 }),
    terminal(ME, "sh3", "w1", "unreachable"),
    workspace(ME, "w2", "empty"),
  ];
  return fleet(records, settings({ machines: [{ id: ME, name: "studio", address: "" }, { id: OTHER, name: "laptop", address: "" }] }));
}

const props = { projects: settings().projects, connected: true };

afterEach(() => vi.unstubAllGlobals());

describe("Sidebar", () => {
  it("shows each machine with its state (offline: one session is unreachable) and numbers", async () => {
    await renderApp(<Sidebar machines={machines()} {...props} />);

    expect(screen.getByRole("heading", { name: "Foundry" })).toBeDefined();
    const studio = screen.getByRole("region", { name: "studio" });
    expect(within(studio).getByText("offline")).toBeDefined();
    expect(within(studio).getByText(/load 0\.42\/core/)).toBeDefined();
    const laptop = screen.getByRole("region", { name: "laptop" });
    expect(within(laptop).getByText("unknown")).toBeDefined();
    expect(within(laptop).getByText("no sample yet")).toBeDefined();
  });

  it("lists a workspace's sessions and shells, each with its state label and a link to its terminal", async () => {
    await renderApp(<Sidebar machines={machines()} {...props} />);

    const group = screen.getByRole("group", { name: "fix login" });
    expect(within(group).getByText(/fix-login/)).toBeDefined();
    expect(within(group).getByText(/demo/)).toBeDefined();
    states.forEach(([, label], i) => {
      const link = within(group).getByRole("link", { name: new RegExp(`model-${i}.*${label}$`) });
      expect(link.getAttribute("href")).toBe(`/terminals/st${i}`);
    });
    expect(within(group).getByRole("link", { name: /Running/ }).getAttribute("href")).toBe("/terminals/sh1");
    expect(within(group).getByRole("link", { name: /Exited 1/ }).getAttribute("href")).toBe("/terminals/sh2");
    expect(within(group).getAllByRole("link", { name: /Unreachable/ })).toHaveLength(2);
    for (const name of ["New shell", "Archive"]) {
      expect(within(group).getByRole("button", { name })).toBeDefined();
    }
    expect(screen.getByRole("group", { name: "empty" })).toBeDefined();
  });

  it("highlights the selected terminal", async () => {
    await renderApp(<Sidebar machines={machines()} selected="sh2" {...props} />);

    expect(screen.getByRole("link", { name: /Exited 1/ }).getAttribute("aria-current")).toBe("true");
    expect(screen.getByRole("link", { name: /Running/ }).getAttribute("aria-current")).toBeNull();
  });

  it("offers a new workspace and says when the events socket is down", async () => {
    await renderApp(<Sidebar machines={[]} {...props} connected={false} />);

    expect(screen.getByRole("button", { name: "New workspace" })).toBeDefined();
    expect(screen.getByText(/disconnected/i)).toBeDefined();
  });

  it("starts a shell in the workspace from New shell", async () => {
    const calls = mockFetch(json(201, { id: "t9" }));
    await renderApp(<Sidebar machines={machines()} {...props} />);

    fireEvent.click(within(screen.getByRole("group", { name: "fix login" })).getByRole("button", { name: "New shell" }));

    await waitFor(() => expect(calls).toEqual([{ url: "/api/workspaces/w1/shells", method: "POST", body: undefined }]));
  });

  it("shows the daemon's words when a shell is refused", async () => {
    mockFetch(json(409, { error: "at the terminal limit" }));
    await renderApp(<Sidebar machines={machines()} {...props} />);

    const group = screen.getByRole("group", { name: "fix login" });
    fireEvent.click(within(group).getByRole("button", { name: "New shell" }));

    expect((await within(group).findByRole("alert")).textContent).toContain("at the terminal limit");
  });
});
