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

const props = { onCreate: () => {} };

afterEach(() => {
  vi.unstubAllGlobals();
  localStorage.clear();
});

describe("Sidebar", () => {
  it("shows each machine with its state (offline: one session is unreachable) and numbers", async () => {
    await renderApp(<Sidebar machines={machines()} {...props} />);

    expect(screen.getByRole("heading", { name: "Foundry" })).toBeDefined();
    const studio = screen.getByRole("region", { name: "studio" });
    expect(within(studio).getByText("offline")).toBeDefined();
    const stats = within(studio).getByText("0.42/core · 12.3 GB mem · 120.5 GB disk · 4 sessions");
    expect(stats.getAttribute("title")).toBe("load 0.42/core  memory 12.3 GB free  disk 120.5 GB free  4 sessions");
    expect(within(studio).getByRole("meter", { name: "Load per core" }).getAttribute("aria-valuenow")).toBe("0.42");
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
    for (const name of ["New session", "New shell", "Archive"]) {
      expect(within(group).getByRole("button", { name })).toBeDefined();
    }
    expect(screen.getByRole("group", { name: "empty" })).toBeDefined();
  });

  it("names each session by its first prompt and says what a working one is doing", async () => {
    const records = [workspace(ME, "w1", "fix login"), terminal(ME, "t1", "w1"), session(ME, "s1", "w1", "t1", "working"), terminal(ME, "t2", "w1"), session(ME, "s2", "w1", "t2", "idle")];
    const briefs = {
      s1: { title: "fix the login bug", now: { kind: "tool" as const, tool: "Edit", text: "Edit src/login.ts" } },
      s2: { title: "write the release notes", now: null },
    };
    await renderApp(<Sidebar machines={fleet(records, settings(), briefs)} {...props} />);

    const group = screen.getByRole("group", { name: "fix login" });
    const working = within(group).getByRole("link", { name: /opus fix the login bug.*Working/ });
    expect(within(working).getByText("Edit src/login.ts")).toBeDefined();
    expect(working.getAttribute("title")).toBe("opus: fix the login bug");
    expect(within(group).getByRole("link", { name: /opus write the release notes.*Idle/ })).toBeDefined();
  });

  it("highlights the selected terminal", async () => {
    await renderApp(<Sidebar machines={machines()} selected="sh2" {...props} />);

    expect(screen.getByRole("link", { name: /Exited 1/ }).getAttribute("aria-current")).toBe("true");
    expect(screen.getByRole("link", { name: /Running/ }).getAttribute("aria-current")).toBeNull();
  });

  it("offers a new workspace", async () => {
    const onCreate = vi.fn();
    await renderApp(<Sidebar machines={[]} {...props} onCreate={onCreate} />);

    fireEvent.click(screen.getByRole("button", { name: "New workspace" }));
    expect(onCreate).toHaveBeenCalled();
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

  it("folds a workspace to one line of its states, the most urgent first, and remembers the fold", async () => {
    const { unmount } = await renderApp(<Sidebar machines={machines()} {...props} />);
    const group = screen.getByRole("group", { name: "fix login" });
    const toggle = within(group).getByRole("button", { name: "fix login" });
    expect(toggle.getAttribute("aria-expanded")).toBe("true");

    fireEvent.click(toggle);

    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    const strip = within(group).getAllByRole("link");
    expect(strip.map((l) => l.getAttribute("aria-label"))).toEqual([
      "model-2 Waiting for you",
      "model-6 Stuck",
      "model-5 Failed",
      "shell Exited 1",
      "model-1 Working",
      "model-0 Starting",
    ]);
    expect(strip[0].getAttribute("href")).toBe("/terminals/st2");
    expect(within(group).getByText("+5")).toBeDefined();
    expect(within(group).queryByRole("button", { name: "New shell" })).toBeNull();
    expect(JSON.parse(localStorage.getItem("foundry.folded") ?? "[]")).toEqual(["w1"]);

    unmount();
    await renderApp(<Sidebar machines={machines()} {...props} />);
    const again = within(screen.getByRole("group", { name: "fix login" })).getByRole("button", { name: "fix login" });
    expect(again.getAttribute("aria-expanded")).toBe("false");
    fireEvent.click(again);
    expect(again.getAttribute("aria-expanded")).toBe("true");
    expect(within(screen.getByRole("group", { name: "fix login" })).getAllByRole("link")).toHaveLength(11);
  });

  it("keeps the open terminal in a folded workspace's line even when more urgent ones fill it", async () => {
    localStorage.setItem("foundry.folded", JSON.stringify(["w1"]));
    await renderApp(<Sidebar machines={machines()} selected="sh1" {...props} />);

    const strip = within(screen.getByRole("group", { name: "fix login" })).getAllByRole("link");
    expect(strip).toHaveLength(6);
    expect(strip[5].getAttribute("aria-label")).toBe("shell Running");
    expect(strip[5].getAttribute("aria-current")).toBe("true");
  });

  it("folds and unfolds every workspace of a machine at once", async () => {
    await renderApp(<Sidebar machines={machines()} {...props} />);
    const studio = screen.getByRole("region", { name: "studio" });

    fireEvent.click(within(studio).getByRole("button", { name: "Fold all" }));
    for (const name of ["fix login", "empty"]) {
      expect(within(studio).getByRole("button", { name }).getAttribute("aria-expanded")).toBe("false");
    }
    fireEvent.click(within(studio).getByRole("button", { name: "Unfold all" }));
    for (const name of ["fix login", "empty"]) {
      expect(within(studio).getByRole("button", { name }).getAttribute("aria-expanded")).toBe("true");
    }
  });
});
