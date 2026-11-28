import { focusManager, useQueryClient } from "@tanstack/react-query";
import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { fleet, withFactory } from "./fleet";
import { Sidebar } from "./Sidebar";
import { CHECKS_EVERY } from "./Shipping";
import { TerminalActions } from "./TerminalMenu";
import { factoryStatuses, ME, onCommit, OTHER, sample, session, settings, terminal, workspace } from "./test-fixtures";
import { json, mockFetch, renderApp } from "./test-utils";
import type { PullRequest, SessionState, Shipping } from "./api";

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
/** The text a sighted reader sees in an element: everything but its screen-reader-only spans. */
function visibleText(el: Element) {
  const copy = el.cloneNode(true) as Element;
  copy.querySelectorAll(".sr-only").forEach((n) => n.remove());
  return copy.textContent ?? "";
}

afterEach(() => {
  vi.unstubAllGlobals();
  localStorage.clear();
});

describe("Sidebar", () => {
  it("marks each workspace with its project's face, so a long list reads by project", async () => {
    await renderApp(<Sidebar machines={machines()} {...props} />);
    const face = within(screen.getByRole("group", { name: "fix login" })).getByText("DE");
    expect(face.getAttribute("title")).toBe("demo");
    // It is a picture of what the row says in words once unfolded, so a screen reader skips it.
    expect(face.getAttribute("aria-hidden")).toBe("true");
  });

  it("hangs a spine from each workspace's face in its most urgent terminal's fill, grey with no terminals", async () => {
    const records = [
      sample(ME, 1),
      workspace(ME, "w1", "asks"),
      terminal(ME, "t1", "w1"),
      session(ME, "s1", "w1", "t1", "working"),
      terminal(ME, "t2", "w1"),
      session(ME, "s2", "w1", "t2", "waiting_for_you"),
      workspace(ME, "w2", "quiet"),
      terminal(ME, "t3", "w2"),
      session(ME, "s3", "w2", "t3", "finished"),
      terminal(ME, "t4", "w2", "unreachable"),
      workspace(ME, "w3", "bare"),
    ];
    const view = fleet(records, settings({ machines: [{ id: ME, name: "studio", address: "" }] }));
    await renderApp(<Sidebar machines={view} {...props} />);
    const spine = (name: string) => screen.getByRole("group", { name }).querySelector<HTMLElement>("[data-spine]")!;

    expect(spine("asks").className).toMatch(/(^| )bg-amber-400( |$)/);
    expect(spine("quiet").className).toMatch(/(^| )bg-neutral-600( |$)/);
    expect(spine("bare").className).toMatch(/(^| )bg-neutral-800( |$)/);
    // It draws what the glyphs already say, so a screen reader skips it.
    expect(spine("bare").getAttribute("aria-hidden")).toBe("true");
    // It stays when the workspace unfolds, still one colour for the whole task.
    unfold("asks");
    expect(spine("asks").className).toMatch(/(^| )bg-amber-400( |$)/);
  });

  it("lays a workspace out as its face's column beside one of everything else, the face and name one fold button with no chevron", async () => {
    await renderApp(<Sidebar machines={machines()} {...props} />);
    const group = screen.getByRole("group", { name: "fix login" });
    const button = within(group).getByRole("button", { name: "fix login" });

    expect(button.getAttribute("aria-expanded")).toBe("false");
    expect(button.getAttribute("title")).toBe("Unfold");
    expect(button.contains(within(group).getByText("DE"))).toBe(true);
    expect(button.querySelector("svg")).toBeNull();
    // Drawn over its own column, left of the content, while it stays part of the button.
    expect(within(group).getByText("DE").className).toMatch(/(^| )size-5( |$)/);
    expect(within(group).getByText("DE").className).toMatch(/(^| )absolute( |$)/);

    unfold("fix login");
    expect(button.getAttribute("aria-expanded")).toBe("true");
    expect(button.getAttribute("title")).toBe("Fold");
    // The name, the standing line, the branch line and the sessions share the content column.
    const content = button.closest<HTMLElement>(".min-w-0:not(button)")!;
    expect(content.contains(within(group).getByText(/fix-login/))).toBe(true);
    const sessions = within(group).getAllByRole("listitem")[0].parentElement!;
    expect(content.contains(sessions)).toBe(true);
    expect(content.contains(group.querySelector("[data-spine]"))).toBe(false);
    // The spine takes the place of the list's own rule, and the rows' own padding lines their glyphs up with the name.
    expect(sessions.className).not.toMatch(/border-l|(^| )-?m[lx]-|(^| )p[lx]-/);
  });

  it("sets the workspaces under a machine 12px apart, so each task reads as one block", async () => {
    await renderApp(<Sidebar machines={machines()} {...props} />);
    const list = screen.getByRole("group", { name: "fix login" }).parentElement!;

    expect(list.className).toMatch(/(^| )(space-y-3|gap-3)( |$)/);
  });

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

  it("lists no workspace, terminal or session of a machine the machine list does not hold", async () => {
    const records = [
      sample(ME, 1),
      workspace(ME, "w1", "fix login"),
      terminal(ME, "t1", "w1"),
      session(ME, "s1", "w1", "t1", "working", { model: "kept-model" }),
      sample(OTHER, 1),
      workspace(OTHER, "w9", "cut off"),
      terminal(OTHER, "t9", "w9"),
      session(OTHER, "s9", "w9", "t9", "working", { model: "gone-model" }),
      terminal(OTHER, "sh9", "w9"),
    ];
    const view = fleet(records, settings({ machines: [{ id: ME, name: "studio", address: "" }] }));
    await renderApp(<Sidebar machines={view} {...props} />);

    expect(screen.getByRole("button", { name: "fix login" })).toBeDefined();
    expect(screen.queryByRole("button", { name: "cut off" })).toBeNull();
    unfold("fix login");
    expect(screen.getByText("kept-model")).toBeDefined();
    expect(screen.queryByText("cut off")).toBeNull();
    expect(screen.queryByText("gone-model")).toBeNull();
    expect(screen.queryByText("shell")).toBeNull();
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
    expect(within(group).getByRole("link", { name: /Open/ }).getAttribute("href")).toBe("/terminals/sh1");
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

  it("offers Archive for a workspace on another machine, which the daemon carries to it, and opens its dialog", async () => {
    mockFetch(json(200, { id: ME }));
    const view = fleet(
      [sample(ME, 1), sample(OTHER, 1), workspace(OTHER, "w9", "remote job"), terminal(OTHER, "t9", "w9", "running")],
      settings({ machines: [{ id: ME, name: "studio", address: "" }, { id: OTHER, name: "laptop", address: "" }] }),
    );
    const noop = () => {};
    await renderApp(
      <TerminalActions machines={view} onOpen={noop} onBeside={noop} onSteps={noop} onChanges={noop} onFind={noop} onNewSession={noop}>
        <Sidebar machines={view} {...props} />
      </TerminalActions>,
    );
    unfold("remote job");

    const group = screen.getByRole("group", { name: "remote job" });
    fireEvent.click(await within(group).findByRole("button", { name: "Archive" }));
    expect(await screen.findByRole("dialog", { name: "Archive remote job?" })).toBeDefined();
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
    expect(visibleText(working)).not.toContain("fix the login bug");
    const idle = within(group).getByRole("link", { name: /opus 2 write the release notes.*Ready/ });
    expect(visibleText(idle)).toContain("write the release notes");
  });

  it("puts each session row on one line, with the step in place of the title while it works", async () => {
    const records = [workspace(ME, "w1", "fix login"), terminal(ME, "t1", "w1"), session(ME, "s1", "w1", "t1", "working"), terminal(ME, "t2", "w1"), session(ME, "s2", "w1", "t2", "idle")];
    const briefs = {
      s1: { title: "fix the login bug", now: { kind: "tool" as const, tool: "Edit", text: "Edit src/login.ts" } },
      s2: { title: "write the release notes", now: null },
    };
    await renderApp(<Sidebar machines={fleet(records, settings(), briefs)} {...props} />);
    unfold("fix login");
    const group = screen.getByRole("group", { name: "fix login" });

    for (const name of [/opus 1/, /opus 2/]) {
      const link = within(group).getByRole("link", { name });
      // One line: the glyph sits centred on it, not at the top of a two-line block.
      expect(link.className).toMatch(/(^| )items-center( |$)/);
      expect(link.className).not.toMatch(/(^| )items-start( |$)/);
      expect(link.className).toMatch(/(^| )py-1\.5( |$)/);
      expect(link.className).toMatch(/(^| )md:py-1( |$)/);
      expect(link.querySelectorAll(".leading-5")).toHaveLength(1);
    }

    const working = within(group).getByRole("link", { name: /opus 1/ });
    const step = within(working).getByText("Edit src/login.ts");
    const hidden = within(working).getByText("fix the login bug");
    expect(hidden.className).toMatch(/(^| )sr-only( |$)/);
    // The title is read before the step, and the step shares the line with the name and the label.
    expect(hidden.compareDocumentPosition(step) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(step.className).toMatch(/(^| )truncate( |$)/);
    expect(step.className).toContain("font-mono");
    const line = working.querySelector(".leading-5")!;
    expect(line.contains(step)).toBe(true);
    expect(line.contains(within(working).getByText("Working"))).toBe(true);
    expect(within(working).getByText("opus 1").className).toMatch(/(^| )text-neutral-500( |$)/);
  });

  it("highlights the selected terminal", async () => {
    await renderApp(<Sidebar machines={machines()} selected="sh2" {...props} />);

    expect(screen.getByRole("link", { name: /Failed · exit 1/ }).getAttribute("aria-current")).toBe("true");
    expect(screen.getByRole("link", { name: /Open/ }).getAttribute("aria-current")).toBeNull();
  });

  it("offers a new task", async () => {
    const onCreate = vi.fn();
    await renderApp(<Sidebar machines={[]} {...props} onCreate={onCreate} />);

    fireEvent.click(screen.getByRole("button", { name: "New task" }));
    expect(onCreate).toHaveBeenCalled();
  });

  it("keeps its header above the scrolling list, with only the title able to give way, so New task never runs past the sidebar's edge", async () => {
    await renderApp(<Sidebar machines={machines()} {...props} />);
    const nav = screen.getByRole("navigation", { name: "Workspaces" });
    const title = screen.getByRole("heading", { name: "Foundry" });
    const list = screen.getByRole("region", { name: "studio" }).parentElement!;
    // The list scrolls on its own, so its scrollbar takes width from the list and not the header.
    expect(list.className).toMatch(/(^| )overflow-y-auto( |$)/);
    expect(list.contains(title)).toBe(false);
    expect(nav.className).not.toMatch(/overflow-/);
    // The buttons keep their size; the title is the part that shortens if the row runs out of room.
    expect(screen.getByRole("button", { name: "New task" }).parentElement!.className).toMatch(/(^| )shrink-0( |$)/);
    expect(title.className).toMatch(/(^| )min-w-0( |$)/);
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
    expect(strip[5].getAttribute("aria-label")).toBe("shell 1 Open");
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

  it("renames a machine in place from its header, its name still opening the machines board", async () => {
    const calls = mockFetch(new Response(null, { status: 204 }));
    await renderApp(<Sidebar machines={machines()} {...props} />);
    const studio = screen.getByRole("region", { name: "studio" });
    expect(within(studio).getByRole("link", { name: "studio" }).getAttribute("href")).toBe("/machines");

    fireEvent.click(within(studio).getByRole("button", { name: "Rename studio" }));
    const field = within(studio).getByRole<HTMLInputElement>("textbox", { name: "Machine name" });
    expect(field.value).toBe("studio");
    fireEvent.change(field, { target: { value: "Tom's desk" } });
    fireEvent.keyDown(field, { key: "Enter" });

    await waitFor(() => expect(within(studio).queryByRole("textbox", { name: "Machine name" })).toBeNull());
    expect(calls).toEqual([{ url: `/api/machines/${ME}/rename`, method: "POST", body: { name: "Tom's desk" } }]);
  });

  it("offers no rename in the header of a machine not in the machine list yet", async () => {
    mockFetch();
    const records = [sample(ME, 1), sample(OTHER, 1)];
    const view = fleet(records, settings({ machines: [{ id: ME, name: "studio", address: "" }] }));
    await renderApp(<Sidebar machines={view} {...props} />);

    const unlisted = screen.getAllByRole("region").find((region) => region.getAttribute("aria-label") !== "studio")!;
    expect(within(unlisted).queryByRole("button", { name: /^Rename/ })).toBeNull();
    expect(within(screen.getByRole("region", { name: "studio" })).getByRole("button", { name: "Rename studio" })).toBeDefined();
  });

  it("shows a refused rename's error under the header's field and sends nothing on Escape", async () => {
    const calls = mockFetch(json(409, { error: 'another machine is named "laptop"' }));
    await renderApp(<Sidebar machines={machines()} {...props} />);
    const studio = screen.getByRole("region", { name: "studio" });

    fireEvent.click(within(studio).getByRole("button", { name: "Rename studio" }));
    const field = within(studio).getByRole<HTMLInputElement>("textbox", { name: "Machine name" });
    fireEvent.change(field, { target: { value: "laptop" } });
    fireEvent.keyDown(field, { key: "Enter" });
    expect((await within(studio).findByRole("alert")).textContent).toBe('another machine is named "laptop"');
    expect(field.value).toBe("laptop");

    fireEvent.keyDown(field, { key: "Escape" });
    expect(within(studio).getByRole("link", { name: "studio" })).toBeDefined();
    expect(calls).toHaveLength(1);
  });
});

/** Project p1's believed status: run 162 builds in w1, run 170 is held up in w2. */
describe("Sidebar and the factory", () => {
  const factoryMachines = () =>
    fleet(
      [sample(ME, 1), workspace(ME, "w1", "fac 162"), terminal(ME, "t1", "w1"), session(ME, "s1", "w1", "t1", "stuck"), onCommit(ME, "w3", "on a commit")],
      settings({ machines: [{ id: ME, name: "studio", address: "" }] }),
    );

  it("marks a factory run's workspace and links it to its issue, and names a quiet session so", async () => {
    await renderApp(<Sidebar machines={withFactory(factoryMachines(), factoryStatuses())} {...props} />);
    const run = screen.getByRole("group", { name: "fac 162" });
    // Folded, the mark shows on the workspace's line.
    expect(within(run).getByRole("link", { name: "factory #162" }).getAttribute("href")).toBe("https://github.com/original-solutions/foundry/issues/162");
    unfold("fac 162");
    expect(within(run).getByText("Quiet 10+ min")).toBeDefined();
    expect(within(screen.getByRole("group", { name: "on a commit" })).queryByRole("link", { name: /^factory/ })).toBeNull();
  });

  it("shows a workspace with no branch by its commit", async () => {
    await renderApp(<Sidebar machines={factoryMachines()} {...props} />);
    unfold("on a commit");
    expect(within(screen.getByRole("group", { name: "on a commit" })).getByText("fedcba9")).toBeDefined();
  });
});

describe("where a workspace's work stands", () => {
  const both = settings({ machines: [{ id: ME, name: "studio", address: "" }, { id: OTHER, name: "laptop", address: "" }] });
  const open: PullRequest = { number: 88, title: "Fix the login form", url: "https://github.com/o/r/pull/88", state: "open", draft: false, checks: "passing", review: "none" };
  const ready: Shipping = { branch: "fix-login", pushed: true, unpushed: 0, pr: open, unreachable: false };

  /** Answers each workspace's `/pr` read from `answers` by its id, and gives back every request sent. */
  function reads(answers: Record<string, () => Response>) {
    const calls: { url: string; method: string; body: unknown }[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init: RequestInit = {}) => {
        calls.push({ url, method: init.method ?? "GET", body: init.body });
        const id = /^\/api\/workspaces\/([^/]+)\/pr$/.exec(url)?.[1];
        const answer = id === undefined ? undefined : answers[id];
        if (!answer) throw new Error(`unexpected request ${url}`);
        return answer();
      }),
    );
    return calls;
  }

  const mine = (state: SessionState = "idle") => [sample(ME, 1), workspace(ME, "w1", "fix login"), terminal(ME, "t1", "w1"), session(ME, "s1", "w1", "t1", state)];
  const theirs = [sample(OTHER, 1), workspace(OTHER, "w9", "remote job"), terminal(OTHER, "t9", "w9"), session(OTHER, "s9", "w9", "t9", "idle")];

  /** The line under a workspace's name: the element holding its project. */
  const lineOf = async (name: string, text: string) => {
    const group = within(screen.getByRole("group", { name }));
    const line = (await group.findByText("demo", { exact: true })).parentElement as HTMLElement;
    await waitFor(() => expect(line.textContent).toBe(text));
    return line;
  };

  it("shows a folded workspace's project and its pull request, linked to GitHub", async () => {
    const calls = reads({ w1: () => json(200, ready) });
    await renderApp(<Sidebar machines={fleet(mine(), both, {}, [ME, OTHER])} {...props} />);
    expect(screen.getByRole("button", { name: "fix login" }).getAttribute("aria-expanded")).toBe("false");

    const line = await lineOf("fix login", "demo · PR #88 · Ready to merge");
    expect(line.getAttribute("title")).toBe("demo · PR #88 · Ready to merge");
    expect(line.className).toContain("truncate");
    const link = within(line).getByRole("link", { name: "PR #88 · Ready to merge" });
    expect(link.getAttribute("href")).toBe("https://github.com/o/r/pull/88");
    expect(link.getAttribute("target")).toBe("_blank");
    expect(link.getAttribute("rel")).toBe("noreferrer");
    expect(link.getAttribute("title")).toBe("Fix the login form (open on GitHub)");
    expect(link.className).toContain("text-emerald-400");
    // Not a stop of its own: the list stays one Tab stop and the arrows pass it by.
    expect(link.getAttribute("tabindex")).toBe("-1");
    expect(link.hasAttribute("data-stop")).toBe(false);
    // The folded line of glyphs is still there beside the name.
    expect(within(screen.getByRole("group", { name: "fix login" })).getByRole("link", { name: /Ready$/ })).toBeDefined();
    expect(calls).toEqual([{ url: "/api/workspaces/w1/pr", method: "POST", body: undefined }]);
  });

  it("shows the same line under an unfolded workspace's name, and its branch alone on the line of actions", async () => {
    reads({ w1: () => json(200, ready) });
    await renderApp(<Sidebar machines={fleet(mine(), both, {}, [ME, OTHER])} {...props} />);
    unfold("fix login");
    const group = within(screen.getByRole("group", { name: "fix login" }));

    const line = await lineOf("fix login", "demo · PR #88 · Ready to merge");
    expect(within(line).getByRole("link", { name: "PR #88 · Ready to merge" }).getAttribute("href")).toBe("https://github.com/o/r/pull/88");
    expect(group.getByText("fix-login", { exact: true })).toBeDefined();
    expect(group.queryByText(/fix-login · demo/)).toBeNull();
    expect(group.getByRole("button", { name: "New shell" })).toBeDefined();
  });

  it("reads a workspace on another online machine with a POST, which the daemon carries to its owner", async () => {
    const calls = reads({ w1: () => json(200, { ...ready, pr: null, pushed: false }), w9: () => json(200, { ...ready, branch: "remote-job", pr: { ...open, number: 307, draft: true, checks: "pending" } }) });
    await renderApp(<Sidebar machines={fleet([...mine(), ...theirs], both, {}, [ME, OTHER])} {...props} />);

    const line = await lineOf("remote job", "demo · Draft PR #307 · Checks running");
    expect(within(line).getByRole("link", { name: "Draft PR #307 · Checks running" }).className).toContain("text-amber-300");
    expect(calls).toContainEqual({ url: "/api/workspaces/w9/pr", method: "POST", body: undefined });
  });

  it("asks nothing for a workspace of an offline machine, and shows its project alone", async () => {
    const calls = reads({ w1: () => json(200, ready) });
    await renderApp(<Sidebar machines={fleet([...mine(), ...theirs], both, {}, [ME])} {...props} />);
    expect(screen.getByRole("region", { name: "laptop" }).textContent).toContain("offline");

    await lineOf("fix login", "demo · PR #88 · Ready to merge");
    const line = await lineOf("remote job", "demo");
    expect(line.getAttribute("title")).toBe("demo");
    expect(calls.map((c) => c.url)).toEqual(["/api/workspaces/w1/pr"]);
  });

  it("shows the project alone, and no error, when the read is refused", async () => {
    const calls = reads({ w9: () => json(400, { error: "no such route" }) });
    await renderApp(<Sidebar machines={fleet(theirs, both, {}, [ME, OTHER])} {...props} />);
    await waitFor(() => expect(calls).toHaveLength(1));

    const line = await lineOf("remote job", "demo");
    expect(within(line).queryByRole("link")).toBeNull();
    expect(screen.queryByRole("alert")).toBeNull();
    expect(screen.queryByText(/no such route|could not/i)).toBeNull();
  });

  it("shows the project alone until the read answers", async () => {
    reads({ w1: () => new Promise<Response>(() => {}) as unknown as Response });
    await renderApp(<Sidebar machines={fleet(mine(), both, {}, [ME, OTHER])} {...props} />);
    await lineOf("fix login", "demo");
  });

  it.each(["working", "starting"] as const)("says Building while a terminal is %s on a branch with no pull request", async (state) => {
    reads({ w1: () => json(200, { ...ready, pr: null, pushed: false }) });
    await renderApp(<Sidebar machines={fleet(mine(state), both, {}, [ME, OTHER])} {...props} />);
    const line = await lineOf("fix login", "demo · Building");
    expect(within(line).queryByRole("link")).toBeNull();
  });

  it("reads again when one of the workspace's terminals changes glyph", async () => {
    const answers = [json(200, { ...ready, pr: null, pushed: false }), json(200, ready)];
    const calls = reads({ w1: () => answers.shift() ?? json(200, ready) });
    const working = fleet(mine("working"), both, {}, [ME, OTHER]);
    const idle = fleet(mine("idle"), both, {}, [ME, OTHER]);
    function Turning() {
      const [view, setView] = useState(working);
      return (
        <>
          <button onClick={() => setView(idle)}>turn ends</button>
          <Sidebar machines={view} {...props} />
        </>
      );
    }
    await renderApp(<Turning />);
    await lineOf("fix login", "demo · Building");
    expect(calls).toHaveLength(1);

    fireEvent.click(screen.getByRole("button", { name: "turn ends" }));
    await lineOf("fix login", "demo · PR #88 · Ready to merge");
    expect(calls.map((c) => [c.url, c.method])).toEqual([
      ["/api/workspaces/w1/pr", "POST"],
      ["/api/workspaces/w1/pr", "POST"],
    ]);
  });

  it("reads only the workspace whose terminal changed glyph, not the others beside it", async () => {
    const calls = reads({ w1: () => json(200, ready), w9: () => json(200, ready) });
    const before = [sample(OTHER, 1), workspace(OTHER, "w9", "remote job"), terminal(OTHER, "t9", "w9"), session(OTHER, "s9", "w9", "t9", "working")];
    await changing(fleet([...mine(), ...before], both, {}, [ME, OTHER]), fleet([...mine(), ...theirs], both, {}, [ME, OTHER]));
    await lineOf("fix login", "demo · PR #88 · Ready to merge");
    await lineOf("remote job", "demo · PR #88 · Ready to merge");
    expect(calls).toHaveLength(2);

    fireEvent.click(screen.getByRole("button", { name: "next state" }));
    await waitFor(() => expect(calls).toHaveLength(3));
    await act(() => new Promise((done) => setTimeout(done, 50)));
    expect(calls.map((c) => c.url)).toEqual(["/api/workspaces/w1/pr", "/api/workspaces/w9/pr", "/api/workspaces/w9/pr"]);
  });

  /** Renders the sidebar with `first`, and a button that swaps in `then`, as the socket's next state does. */
  async function changing(first: ReturnType<typeof fleet>, then: ReturnType<typeof fleet>, beside?: (view: ReturnType<typeof fleet>) => ReactNode, after = false) {
    function Changing() {
      const [view, setView] = useState(first);
      return (
        <>
          <button onClick={() => setView(then)}>next state</button>
          {!after && beside?.(view)}
          <Sidebar machines={view} {...props} />
          {after && beside?.(view)}
        </>
      );
    }
    await renderApp(<Changing />);
  }

  /** Another reader of the shared entry, as a board card is: it marks it out of date when w1's glyphs change. */
  function AlsoInvalidates({ view }: { view: ReturnType<typeof fleet> }) {
    const client = useQueryClient();
    const glyphs = JSON.stringify(view);
    const last = useRef(glyphs);
    useEffect(() => {
      if (last.current === glyphs) return;
      last.current = glyphs;
      void client.invalidateQueries({ queryKey: ["pr", "w1"], exact: true });
    }, [glyphs, client]);
    return null;
  }

  it.each([
    ["before", false],
    ["after", true],
  ])("sends one read for a glyph change that another reader of the entry, %s the sidebar, also marks out of date", async (_, after) => {
    const answers = [json(200, { ...ready, pr: null, pushed: false })];
    const calls = reads({ w1: () => answers.shift() ?? json(200, ready) });
    await changing(fleet(mine("working"), both, {}, [ME, OTHER]), fleet(mine("idle"), both, {}, [ME, OTHER]), (view) => <AlsoInvalidates view={view} />, after);
    await lineOf("fix login", "demo · Building");

    fireEvent.click(screen.getByRole("button", { name: "next state" }));
    await lineOf("fix login", "demo · PR #88 · Ready to merge");
    await act(() => new Promise((done) => setTimeout(done, 50)));
    expect(calls).toHaveLength(2);
  });

  it("reads again after a glyph change that lands while an earlier read is still on its way", async () => {
    let release: (answer: Response) => void = () => {};
    const held = new Promise<Response>((done) => (release = done));
    const answers: (() => Response | Promise<Response>)[] = [() => held, () => json(200, ready)];
    const calls = reads({ w1: () => (answers.shift() ?? (() => json(200, ready)))() as Response });
    await changing(fleet(mine("working"), both, {}, [ME, OTHER]), fleet(mine("idle"), both, {}, [ME, OTHER]));
    await waitFor(() => expect(calls).toHaveLength(1));

    // The turn ends (it pushed and opened #88) while the read from before it is still out.
    fireEvent.click(screen.getByRole("button", { name: "next state" }));
    await act(() => new Promise((done) => setTimeout(done, 20)));
    release(json(200, { ...ready, pr: null }));

    await lineOf("fix login", "demo · PR #88 · Ready to merge");
    expect(calls).toHaveLength(2);
  });

  it("shows the project alone again when a later read fails", async () => {
    const answers = [json(200, ready), json(503, { error: "laptop did not answer" })];
    const calls = reads({ w1: () => answers.shift() ?? json(503, {}) });
    await changing(fleet(mine("working"), both, {}, [ME, OTHER]), fleet(mine("idle"), both, {}, [ME, OTHER]));
    await lineOf("fix login", "demo · PR #88 · Ready to merge");

    fireEvent.click(screen.getByRole("button", { name: "next state" }));
    await waitFor(() => expect(calls).toHaveLength(2));
    await lineOf("fix login", "demo");
    expect(screen.queryByText(/did not answer/)).toBeNull();
  });

  it("shows the project alone once the workspace's machine goes offline", async () => {
    const calls = reads({ w9: () => json(200, ready) });
    await changing(fleet(theirs, both, {}, [ME, OTHER]), fleet(theirs, both, {}, [ME]));
    await lineOf("remote job", "demo · PR #88 · Ready to merge");

    fireEvent.click(screen.getByRole("button", { name: "next state" }));
    await lineOf("remote job", "demo");
    expect(calls).toHaveLength(1);
  });

  it("does not read again when the window takes focus", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      const calls = reads({ w1: () => json(200, ready) });
      await renderApp(<Sidebar machines={fleet(mine(), both, {}, [ME, OTHER])} {...props} />);
      await lineOf("fix login", "demo · PR #88 · Ready to merge");
      await act(() => vi.advanceTimersByTimeAsync(5 * 60_000));
      act(() => {
        focusManager.setFocused(false);
        focusManager.setFocused(true);
      });
      await act(() => vi.advanceTimersByTimeAsync(1_000));
      expect(calls).toHaveLength(1);
    } finally {
      focusManager.setFocused(undefined);
      vi.useRealTimers();
    }
  });

  it("reads a row only once it is on screen", async () => {
    const watched: { fire: (on: boolean) => void }[] = [];
    vi.stubGlobal(
      "IntersectionObserver",
      class {
        constructor(private callback: (entries: { isIntersecting: boolean }[]) => void) {}
        observe() {
          watched.push({ fire: (on) => this.callback([{ isIntersecting: on }]) });
        }
        disconnect() {}
      },
    );
    const calls = reads({ w1: () => json(200, ready) });
    await renderApp(<Sidebar machines={fleet(mine(), both, {}, [ME, OTHER])} {...props} />);
    await act(() => new Promise((done) => setTimeout(done, 20)));
    expect(calls).toEqual([]);
    await lineOf("fix login", "demo");

    act(() => watched.forEach((w) => w.fire(true)));
    await lineOf("fix login", "demo · PR #88 · Ready to merge");
    expect(calls).toHaveLength(1);
  });

  it("reads an open pull request again on the timer only while its checks run", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      const pending: Shipping = { ...ready, pr: { ...open, checks: "pending" } };
      const answers = [json(200, pending), json(200, ready)];
      const calls = reads({ w1: () => answers.shift() ?? json(200, ready) });
      await renderApp(<Sidebar machines={fleet(mine(), both, {}, [ME, OTHER])} {...props} />);
      await lineOf("fix login", "demo · PR #88 · Checks running");

      await act(() => vi.advanceTimersByTimeAsync(CHECKS_EVERY));
      await lineOf("fix login", "demo · PR #88 · Ready to merge");
      expect(calls).toHaveLength(2);

      await act(() => vi.advanceTimersByTimeAsync(CHECKS_EVERY * 3));
      expect(calls).toHaveLength(2);
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("renaming a session", () => {
  const noop = () => {};
  const brief = { s1: { title: "fix the login redirect", now: null } };

  function view(name?: string) {
    const records = [
      sample(ME, 1),
      workspace(ME, "w1", "fix login"),
      terminal(ME, "t1", "w1", "running"),
      session(ME, "s1", "w1", "t1", "idle", { model: "sonnet", name }),
      terminal(ME, "sh1", "w1", "running"),
    ];
    return fleet(records, settings({ machines: [{ id: ME, name: "studio", address: "" }] }), brief);
  }

  /** The workspace's group in a sidebar with terminal t1 open, inside the terminals' actions. */
  async function sidebar(machines = view()) {
    await renderApp(
      <TerminalActions machines={machines} onOpen={noop} onBeside={noop} onSteps={noop} onChanges={noop} onFind={noop} onNewSession={noop}>
        <Sidebar machines={machines} {...props} selected="t1" />
      </TerminalActions>,
    );
    return within(screen.getByRole("group", { name: "fix login" }));
  }

  const renames = (calls: { url: string }[]) => calls.filter((c) => c.url.endsWith("/rename"));
  /** Lets the page settle once a field opens, after which leaving it is the person's. */
  const settle = () => act(() => new Promise((done) => setTimeout(done, 0)));

  it("swaps a session's name for a field on a double-click, holding the name all selected, and Enter saves it trimmed", async () => {
    const calls = mockFetch(json(200, { id: ME }), new Response(null, { status: 204 }));
    const group = await sidebar();
    const row = group.getByRole("link", { name: /^sonnet fix the login redirect/ });
    expect(row.getAttribute("href")).toBe("/terminals/t1");

    fireEvent.doubleClick(row);
    const field = group.getByRole<HTMLInputElement>("textbox", { name: "Session name" });
    expect(field.value).toBe("fix the login redirect");
    expect(document.activeElement).toBe(field);
    expect([field.selectionStart, field.selectionEnd]).toEqual([0, field.value.length]);
    fireEvent.change(field, { target: { value: "  login redirect " } });
    fireEvent.keyDown(field, { key: "Enter" });

    await waitFor(() => expect(group.queryByRole("textbox", { name: "Session name" })).toBeNull());
    expect(renames(calls)).toEqual([{ url: "/api/sessions/s1/rename", method: "POST", body: { name: "login redirect" } }]);
  });

  it("gives a shell no field on a double-click", async () => {
    mockFetch(json(200, { id: ME }));
    const group = await sidebar();
    fireEvent.doubleClick(group.getByRole("link", { name: /^shell/ }));
    expect(group.queryByRole("textbox")).toBeNull();
  });

  it("puts the name back on Escape, and on leaving the field unchanged, sending nothing", async () => {
    const calls = mockFetch(json(200, { id: ME }));
    const group = await sidebar();

    fireEvent.doubleClick(group.getByRole("link", { name: /^sonnet/ }));
    fireEvent.change(group.getByRole("textbox", { name: "Session name" }), { target: { value: "something else" } });
    fireEvent.keyDown(group.getByRole("textbox", { name: "Session name" }), { key: "Escape" });
    expect(group.queryByRole("textbox")).toBeNull();

    fireEvent.doubleClick(group.getByRole("link", { name: /^sonnet/ }));
    const field = group.getByRole<HTMLInputElement>("textbox", { name: "Session name" });
    expect(field.value).toBe("fix the login redirect");
    await settle();
    fireEvent.blur(field);
    expect(group.queryByRole("textbox")).toBeNull();
    expect(group.getByRole("link", { name: /^sonnet fix the login redirect/ })).toBeDefined();
    expect(renames(calls)).toEqual([]);
  });

  it("keeps the field open and takes the keyboard back when something else takes it as the field opens", async () => {
    mockFetch(json(200, { id: ME }));
    const group = await sidebar();
    fireEvent.doubleClick(group.getByRole("link", { name: /^sonnet/ }));
    const field = group.getByRole("textbox", { name: "Session name" });
    const terminal = document.createElement("textarea");
    document.body.append(terminal);
    terminal.focus();
    fireEvent.blur(field);
    await settle();

    expect(group.getByRole("textbox", { name: "Session name" })).toBe(field);
    expect(document.activeElement).toBe(field);
    terminal.remove();
  });

  it("saves a changed name when the field is left", async () => {
    const calls = mockFetch(json(200, { id: ME }), new Response(null, { status: 204 }));
    const group = await sidebar();
    fireEvent.doubleClick(group.getByRole("link", { name: /^sonnet/ }));
    const field = group.getByRole("textbox", { name: "Session name" });
    fireEvent.change(field, { target: { value: "login redirect" } });
    await settle();
    fireEvent.blur(field);

    await waitFor(() => expect(group.queryByRole("textbox")).toBeNull());
    expect(renames(calls)).toEqual([{ url: "/api/sessions/s1/rename", method: "POST", body: { name: "login redirect" } }]);
  });

  it("clears a given name with Enter on an empty field, so the first prompt shows again", async () => {
    const calls = mockFetch(json(200, { id: ME }), new Response(null, { status: 204 }));
    const group = await sidebar(view("login redirect"));
    const row = group.getByRole("link", { name: /^sonnet login redirect/ });

    fireEvent.doubleClick(row);
    const field = group.getByRole<HTMLInputElement>("textbox", { name: "Session name" });
    expect(field.value).toBe("login redirect");
    fireEvent.change(field, { target: { value: "" } });
    fireEvent.keyDown(field, { key: "Enter" });

    await waitFor(() => expect(group.queryByRole("textbox")).toBeNull());
    expect(renames(calls)).toEqual([{ url: "/api/sessions/s1/rename", method: "POST", body: { name: "" } }]);
  });

  it("shows a refused rename's error beside the field and keeps the text", async () => {
    mockFetch(json(200, { id: ME }), json(503, { error: "build-box is offline" }));
    const group = await sidebar();
    fireEvent.doubleClick(group.getByRole("link", { name: /^sonnet/ }));
    const field = group.getByRole<HTMLInputElement>("textbox", { name: "Session name" });
    fireEvent.change(field, { target: { value: "login redirect" } });
    fireEvent.keyDown(field, { key: "Enter" });

    expect((await group.findByRole("alert")).textContent).toBe("build-box is offline");
    expect(field.value).toBe("login redirect");
  });
});
