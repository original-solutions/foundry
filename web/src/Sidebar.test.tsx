import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { fleet } from "./fleet";
import { Sidebar } from "./Sidebar";
import { TerminalActions } from "./TerminalMenu";
import { ME, OTHER, sample, session, settings, terminal, workspace } from "./test-fixtures";
import { json, mockFetch, renderApp } from "./test-utils";
import type { SessionState } from "./api";

const states: [SessionState, string][] = [
  ["starting", "Starting"],
  ["working", "Working"],
  ["waiting_for_you", "Needs you"],
  ["idle", "Ready"],
  ["finished", "Ended"],
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

/** Unfolds a workspace by its name, as a click on it does. */
const unfold = (name: string) => fireEvent.click(screen.getByRole("button", { name }));

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
    const meter = within(studio).getByRole("meter", { name: "Load per core" });
    expect(meter.getAttribute("aria-valuenow")).toBe("0.42");
    // Load with room to spare is calm grey; only a machine near or past full takes a colour.
    expect(meter.firstElementChild?.getAttribute("class")).toContain("bg-neutral-");
    const laptop = screen.getByRole("region", { name: "laptop" });
    expect(within(laptop).getByText("unknown")).toBeDefined();
    expect(within(laptop).getByText("no sample yet")).toBeDefined();
  });

  it("lists a workspace's sessions and shells, each with its state label and a link to its terminal", async () => {
    mockFetch(json(200, { id: ME }));
    const onNewSession = vi.fn();
    const view = machines();
    const noop = () => {};
    await renderApp(
      <TerminalActions machines={view} onOpen={noop} onBeside={noop} onSteps={noop} onChanges={noop} onFind={noop} onNewSession={onNewSession}>
        <Sidebar machines={view} {...props} />
      </TerminalActions>,
    );
    unfold("fix login");

    const group = screen.getByRole("group", { name: "fix login" });
    expect(within(group).getByText(/fix-login/)).toBeDefined();
    expect(within(group).getByText(/demo/)).toBeDefined();
    states.forEach(([, label], i) => {
      const link = within(group).getByRole("link", { name: new RegExp(`model-${i}.*${label}$`) });
      expect(link.getAttribute("href")).toBe(`/terminals/st${i}`);
    });
    expect(within(group).getByRole("link", { name: /Running/ }).getAttribute("href")).toBe("/terminals/sh1");
    expect(within(group).getByRole("link", { name: /Failed · exit 1/ }).getAttribute("href")).toBe("/terminals/sh2");
    expect(within(group).getAllByRole("link", { name: /Unreachable/ })).toHaveLength(2);
    for (const name of ["New session", "New shell"]) {
      expect(within(group).getByRole("button", { name })).toBeDefined();
    }
    // Archiving is offered once the UI knows the workspace is on its own machine.
    expect(await within(group).findByRole("button", { name: "Archive" })).toBeDefined();
    // The layout asks for the model and lands in the session once it starts.
    fireEvent.click(within(group).getByRole("button", { name: "New session" }));
    expect(onNewSession).toHaveBeenCalledWith("w1", "fix login");
    expect(screen.getByRole("group", { name: "empty" })).toBeDefined();
  });

  it("names each session by its first prompt and says what a working one is doing", async () => {
    const records = [workspace(ME, "w1", "fix login"), terminal(ME, "t1", "w1"), session(ME, "s1", "w1", "t1", "working"), terminal(ME, "t2", "w1"), session(ME, "s2", "w1", "t2", "idle")];
    const briefs = {
      s1: { title: "fix the login bug", now: { kind: "tool" as const, tool: "Edit", text: "Edit src/login.ts" } },
      s2: { title: "write the release notes", now: null },
    };
    await renderApp(<Sidebar machines={fleet(records, settings(), briefs)} {...props} />);
    unfold("fix login");

    const group = screen.getByRole("group", { name: "fix login" });
    const working = within(group).getByRole("link", { name: /opus 1 fix the login bug.*Working/ });
    expect(within(working).getByText("Edit src/login.ts")).toBeDefined();
    expect(working.getAttribute("title")).toBe("opus 1: fix the login bug");
    expect(within(group).getByRole("link", { name: /opus 2 write the release notes.*Ready/ })).toBeDefined();
  });

  it("highlights the selected terminal", async () => {
    await renderApp(<Sidebar machines={machines()} selected="sh2" {...props} />);

    expect(screen.getByRole("link", { name: /Failed · exit 1/ }).getAttribute("aria-current")).toBe("true");
    expect(screen.getByRole("link", { name: /Running/ }).getAttribute("aria-current")).toBeNull();
  });

  it("offers a new task", async () => {
    const onCreate = vi.fn();
    await renderApp(<Sidebar machines={[]} {...props} onCreate={onCreate} />);

    fireEvent.click(screen.getByRole("button", { name: "New task" }));
    expect(onCreate).toHaveBeenCalled();
  });

  it("starts a shell in the workspace from New shell", async () => {
    const calls = mockFetch(json(201, { id: "t9" }));
    await renderApp(<Sidebar machines={machines()} {...props} />);
    unfold("fix login");

    fireEvent.click(within(screen.getByRole("group", { name: "fix login" })).getByRole("button", { name: "New shell" }));

    await waitFor(() => expect(calls).toEqual([{ url: "/api/workspaces/w1/shells", method: "POST", body: undefined }]));
  });

  it("shows the daemon's words when a shell is refused", async () => {
    mockFetch(json(409, { error: "at the terminal limit" }));
    await renderApp(<Sidebar machines={machines()} {...props} />);
    unfold("fix login");

    const group = screen.getByRole("group", { name: "fix login" });
    fireEvent.click(within(group).getByRole("button", { name: "New shell" }));

    expect((await within(group).findByRole("alert")).textContent).toBe("Could not start a shell: at the terminal limit.");
  });

  it("shows a workspace as one line of its states, the most urgent first, until it is unfolded, and remembers the unfold", async () => {
    const { unmount } = await renderApp(<Sidebar machines={machines()} {...props} />);
    const group = screen.getByRole("group", { name: "fix login" });
    const toggle = within(group).getByRole("button", { name: "fix login" });
    expect(toggle.getAttribute("aria-expanded")).toBe("false");

    const strip = within(group).getAllByRole("link");
    expect(strip.map((l) => l.getAttribute("aria-label"))).toEqual([
      "model-2 Needs you",
      "model-6 Stuck",
      "model-5 Failed",
      "shell 2 Failed · exit 1",
      "model-1 Working",
      "model-0 Starting",
    ]);
    expect(strip[0].getAttribute("href")).toBe("/terminals/st2");
    expect(within(group).getByText("+5")).toBeDefined();
    expect(within(group).queryByRole("button", { name: "New shell" })).toBeNull();

    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    expect(within(group).getAllByRole("link")).toHaveLength(11);
    expect(JSON.parse(localStorage.getItem("foundry.unfolded") ?? "[]")).toEqual(["w1"]);

    unmount();
    await renderApp(<Sidebar machines={machines()} {...props} />);
    const again = within(screen.getByRole("group", { name: "fix login" })).getByRole("button", { name: "fix login" });
    expect(again.getAttribute("aria-expanded")).toBe("true");
    fireEvent.click(again);
    expect(again.getAttribute("aria-expanded")).toBe("false");
  });

  it("opens the workspace you are in, and folds it again when you leave it", async () => {
    const view = fleet(
      [
        workspace(ME, "w1", "alpha"),
        terminal(ME, "t1", "w1"),
        session(ME, "s1", "w1", "t1", "idle"),
        workspace(ME, "w2", "beta"),
        terminal(ME, "sh2", "w2", "running"),
        workspace(ME, "w3", "gamma"),
        terminal(ME, "sh3", "w3", "running"),
      ],
      settings(),
    );
    function Moving() {
      const [selected, setSelected] = useState<string>();
      return (
        <>
          <button onClick={() => setSelected("t1")}>go alpha</button>
          <button onClick={() => setSelected("sh2")}>go beta</button>
          <button onClick={() => setSelected(undefined)}>go board</button>
          <Sidebar machines={view} selected={selected} {...props} />
        </>
      );
    }
    await renderApp(<Moving />);
    const open = (name: string) => screen.getByRole("button", { name }).getAttribute("aria-expanded");
    // The board: every workspace is one line, as the board itself has the detail.
    expect([open("alpha"), open("beta"), open("gamma")]).toEqual(["false", "false", "false"]);

    unfold("gamma");
    fireEvent.click(screen.getByRole("button", { name: "go alpha" }));
    expect([open("alpha"), open("beta"), open("gamma")]).toEqual(["true", "false", "true"]);

    fireEvent.click(screen.getByRole("button", { name: "go beta" }));
    expect([open("alpha"), open("beta"), open("gamma")]).toEqual(["false", "true", "true"]);

    // Folding the one you are in holds while you stay in it.
    unfold("beta");
    expect(open("beta")).toBe("false");

    fireEvent.click(screen.getByRole("button", { name: "go board" }));
    expect([open("alpha"), open("beta"), open("gamma")]).toEqual(["false", "false", "true"]);
  });

  it("keeps the open terminal in a folded workspace's line even when more urgent ones fill it", async () => {
    await renderApp(<Sidebar machines={machines()} selected="sh1" {...props} />);
    unfold("fix login");

    const strip = within(screen.getByRole("group", { name: "fix login" })).getAllByRole("link");
    expect(strip).toHaveLength(6);
    expect(strip[5].getAttribute("aria-label")).toBe("shell 1 Running");
    expect(strip[5].getAttribute("aria-current")).toBe("true");
  });

  it("unfolds and folds every workspace of a machine at once", async () => {
    await renderApp(<Sidebar machines={machines()} {...props} />);
    const studio = screen.getByRole("region", { name: "studio" });

    fireEvent.click(within(studio).getByRole("button", { name: "Unfold all" }));
    for (const name of ["fix login", "empty"]) {
      expect(within(studio).getByRole("button", { name }).getAttribute("aria-expanded")).toBe("true");
    }
    fireEvent.click(within(studio).getByRole("button", { name: "Fold all" }));
    for (const name of ["fix login", "empty"]) {
      expect(within(studio).getByRole("button", { name }).getAttribute("aria-expanded")).toBe("false");
    }
  });
  describe("with the keyboard", () => {
    const pair = () =>
      fleet(
        [
          workspace(ME, "w1", "alpha"),
          terminal(ME, "t1", "w1"),
          session(ME, "s1", "w1", "t1", "idle"),
          terminal(ME, "sh1", "w1", "running"),
          workspace(ME, "w2", "beta"),
          terminal(ME, "sh2", "w2", "running"),
        ],
        settings(),
      );
    const ws = (name: string) => screen.getByRole("button", { name });
    const link = (href: string) => document.querySelector<HTMLElement>(`a[href="${href}"]`)!;
    const press = (key: string) => fireEvent.keyDown(document.activeElement!, { key });

    it("is one Tab stop, and the arrows walk its workspaces and terminals", async () => {
      await renderApp(<Sidebar machines={pair()} {...props} />);
      unfold("alpha");
      unfold("beta");

      const stops = () => [...document.querySelectorAll("[data-stop]")].filter((e) => (e as HTMLElement).tabIndex === 0);
      expect(stops()).toEqual([ws("alpha")]);
      expect(link("/terminals/t1").tabIndex).toBe(-1);

      ws("alpha").focus();
      press("ArrowDown");
      expect(document.activeElement).toBe(link("/terminals/t1"));
      press("ArrowDown");
      expect(document.activeElement).toBe(link("/terminals/sh1"));
      press("ArrowDown");
      expect(document.activeElement).toBe(ws("beta"));
      press("ArrowUp");
      expect(document.activeElement).toBe(link("/terminals/sh1"));
      // The one focused last is the Tab stop, so Tab comes back to it.
      expect(stops()).toEqual([link("/terminals/sh1")]);
      press("End");
      expect(document.activeElement).toBe(link("/terminals/sh2"));
      press("Home");
      expect(document.activeElement).toBe(ws("alpha"));
    });

    it("folds with Left, unfolds with Right, and Left from a terminal goes to its workspace", async () => {
      await renderApp(<Sidebar machines={pair()} {...props} />);
      unfold("alpha");

      link("/terminals/sh1").focus();
      press("ArrowLeft");
      expect(document.activeElement).toBe(ws("alpha"));
      press("ArrowLeft");
      expect(ws("alpha").getAttribute("aria-expanded")).toBe("false");
      press("ArrowRight");
      expect(ws("alpha").getAttribute("aria-expanded")).toBe("true");
      press("ArrowRight");
      expect(document.activeElement).toBe(link("/terminals/t1"));
    });

    it("jumps to the next one whose name starts with what is typed", async () => {
      await renderApp(<Sidebar machines={pair()} {...props} />);

      ws("alpha").focus();
      press("b");
      expect(document.activeElement).toBe(ws("beta"));
      press("o");
      // "bo" matches nothing, so the keyboard stays.
      expect(document.activeElement).toBe(ws("beta"));
    });

    it("makes the open terminal the Tab stop", async () => {
      await renderApp(<Sidebar machines={pair()} selected="sh2" {...props} />);

      expect(link("/terminals/sh2").tabIndex).toBe(0);
      expect(ws("alpha").tabIndex).toBe(-1);
    });

    it("offers the list of keys", async () => {
      const onKeys = vi.fn();
      await renderApp(<Sidebar machines={pair()} {...props} onKeys={onKeys} />);

      fireEvent.click(screen.getByRole("button", { name: /Keyboard/ }));
      expect(onKeys).toHaveBeenCalled();
    });
  });
});
