import { cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Changes } from "./api";
import { ArchiveDialog } from "./Archive";
import { fleet, type WorkspaceView } from "./fleet";
import { dangerButton, primaryButton } from "./Modal";
import { ME, OTHER, session, settings, terminal, workspace } from "./test-fixtures";
import { json, renderApp, type Call } from "./test-utils";

afterEach(() => vi.unstubAllGlobals());

const file = (path: string, added: number, removed: number, uncommitted = false) => ({
  path,
  from: null,
  status: "modified" as const,
  added,
  removed,
  uncommitted,
  patch: "",
});

const changes: Changes = {
  base: "main",
  commits: [
    { hash: "abc1234", subject: "Check the session on login" },
    { hash: "def5678", subject: "Add the login form" },
  ],
  files: [file("src/login.ts", 30, 4), file("README.md", 2, 0, true)],
};

/** Workspace "fix login" with a working session, an idle one and an exited shell. */
function fixLogin(): WorkspaceView {
  const records = [
    workspace(ME, "w1", "fix login"),
    terminal(ME, "t1", "w1"),
    session(ME, "s1", "w1", "t1", "working"),
    terminal(ME, "t2", "w1"),
    session(ME, "s2", "w1", "t2", "idle", { model: "sonnet" }),
    terminal(ME, "t3", "w1", "exited", { exit_code: 0 }),
  ];
  return fleet(records, settings(), { s1: { title: "fix the login bug", now: null } })[0].workspaces[0];
}

/** Answers the changes read with `read` and each archive with the next of `archives`; gives back every request. */
function stub(read: () => Response, ...archives: Response[]): Call[] {
  const calls: Call[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init: RequestInit = {}) => {
      calls.push({ url, method: init.method ?? "GET", body: init.body ? JSON.parse(String(init.body)) : undefined });
      if (url === "/api/workspaces/w1/changes") return read();
      const next = archives.shift();
      if (!next) throw new Error("unexpected request");
      return next;
    }),
  );
  return calls;
}

const posts = (calls: Call[]) => calls.filter((c) => c.method === "POST");

const studio = { id: ME, name: "studio" };
const attic = { id: OTHER, name: "attic" };

/** Shows the dialog for "fix login" on `machine`, from the UI `here` serves (unknown when undefined). */
async function show(onClose = vi.fn(), onArchived = vi.fn(), machine = studio, here: string | undefined = ME) {
  await renderApp(<ArchiveDialog workspace={fixLogin()} machine={machine} here={here} onClose={onClose} onArchived={onArchived} />);
  return screen.findByRole("dialog", { name: "Archive fix login?" });
}

/** The same dialog for "fix login" on another machine, attic. */
const showElsewhere = (onClose = vi.fn(), onArchived = vi.fn()) => show(onClose, onArchived, attic, ME);
const reads = (calls: Call[]) => calls.filter((c) => c.url.endsWith("/changes"));

describe("ArchiveDialog", () => {
  it("shows what stays on the branch, what is lost with the worktree and what stops, before anything is archived", async () => {
    const calls = stub(() => json(200, changes));
    const dialog = await show();

    expect(dialog.textContent).toMatch(/terminals stop/);
    expect(dialog.textContent).toContain("fix-login");

    const kept = await within(dialog).findByRole("region", { name: "Kept on fix-login" });
    expect(kept.textContent).toContain("abc1234");
    expect(kept.textContent).toContain("Check the session on login");
    expect(kept.textContent).toContain("Add the login form");

    const lost = within(dialog).getByRole("region", { name: "Lost with the worktree" });
    expect(lost.textContent).toContain("README.md");
    expect(lost.textContent).not.toContain("src/login.ts");

    const stops = within(dialog).getByRole("region", { name: "Stops" });
    expect(within(stops).getAllByRole("listitem").map((li) => li.textContent)).toEqual([
      expect.stringMatching(/opus.*fix the login bug.*Working.*mid-turn/),
      expect.stringMatching(/sonnet.*Ready/),
      expect.stringMatching(/shell.*Ended/),
    ]);
    expect(posts(calls)).toEqual([]);
  });

  it("says so when every change is committed, and when there is nothing to keep", async () => {
    stub(() => json(200, { ...changes, files: [file("src/login.ts", 30, 4)] }));
    const dialog = await show();
    expect(await within(dialog).findByText("Every change is committed.")).toBeDefined();
    expect(within(dialog).queryByRole("region", { name: "Lost with the worktree" })).toBeNull();
  });

  it("says when the branch has no commits since its base", async () => {
    stub(() => json(200, { base: "main", commits: [], files: [] }));
    const dialog = await show();
    const kept = await within(dialog).findByRole("region", { name: "Kept on fix-login" });
    await waitFor(() => expect(kept.textContent).toMatch(/No commits since main/));
  });

  it("confirms with the shared danger button while work would be lost or cut, and the shared primary one otherwise", async () => {
    stub(() => json(200, changes));
    let dialog = await show();
    expect(await within(dialog).findByText("README.md")).toBeDefined();
    expect(within(dialog).getByRole("button", { name: "Archive" }).className).toBe(dangerButton);
    cleanup();

    stub(() => json(200, { ...changes, files: [file("src/login.ts", 30, 4)] }));
    const quiet = fleet([workspace(ME, "w1", "fix login"), terminal(ME, "t1", "w1"), session(ME, "s1", "w1", "t1", "idle")], settings())[0].workspaces[0];
    await renderApp(<ArchiveDialog workspace={quiet} machine={studio} here={ME} onClose={vi.fn()} onArchived={vi.fn()} />);
    dialog = await screen.findByRole("dialog", { name: "Archive fix login?" });
    expect(await within(dialog).findByText("Every change is committed.")).toBeDefined();
    expect(within(dialog).getByRole("button", { name: "Archive" }).className).toBe(primaryButton);
  });

  it("still archives when its changes cannot be read, and says they could not be", async () => {
    const calls = stub(() => json(500, { error: "git broke" }), new Response(null, { status: 204 }));
    const dialog = await show();
    expect(await within(dialog).findByText(/can.t be read from here/)).toBeDefined();
    fireEvent.click(within(dialog).getByRole("button", { name: "Archive" }));
    await waitFor(() => expect(posts(calls)).toHaveLength(1));
  });

  it("archives without force once confirmed, and says what it kept", async () => {
    const onArchived = vi.fn();
    const onClose = vi.fn();
    const calls = stub(() => json(200, changes), new Response(null, { status: 204 }));
    const dialog = await show(onClose, onArchived);

    fireEvent.click(within(dialog).getByRole("button", { name: "Archive" }));

    await waitFor(() => expect(onArchived).toHaveBeenCalledWith("fix login", "fix-login"));
    expect(posts(calls)).toEqual([{ url: "/api/workspaces/w1/archive", method: "POST", body: { force: false } }]);
    expect(onClose).toHaveBeenCalled();
  });

  it("lists what holds an archive up on 409 and archives anyway with force", async () => {
    const onArchived = vi.fn();
    const calls = stub(
      () => json(200, changes),
      json(409, { error: "workspace has unsaved work", uncommitted: ["src/a.rs"], unpushed: ["abc123 wip"], submodules: ["vendor/x"] }),
      new Response(null, { status: 204 }),
    );
    const dialog = await show(vi.fn(), onArchived);

    fireEvent.click(within(dialog).getByRole("button", { name: "Archive" }));
    const held = await within(dialog).findByRole("region", { name: "Archiving now would lose this work" });
    expect(held.textContent).toContain("src/a.rs");
    expect(held.textContent).toContain("abc123 wip");
    expect(held.textContent).toContain("vendor/x");
    expect(within(dialog).getByRole("button", { name: "Archive anyway" }).className).toBe(dangerButton);

    fireEvent.click(within(dialog).getByRole("button", { name: "Archive anyway" }));

    await waitFor(() => expect(onArchived).toHaveBeenCalled());
    expect(posts(calls).map((c) => c.body)).toEqual([{ force: false }, { force: true }]);
  });

  it("shows the text of any other error in the dialog", async () => {
    stub(() => json(200, changes), json(500, { error: "disk on fire" }));
    const dialog = await show();

    fireEvent.click(within(dialog).getByRole("button", { name: "Archive" }));

    expect((await within(dialog).findByRole("alert")).textContent).toBe("Could not archive fix login: disk on fire.");
  });

  it("reads and shows the changes while the machine serving the UI is not known yet", async () => {
    const calls = stub(() => json(200, changes));
    const dialog = await show(vi.fn(), vi.fn(), attic, undefined);
    const kept = await within(dialog).findByRole("region", { name: "Kept on fix-login" });
    await waitFor(() => expect(kept.textContent).toContain("Check the session on login"));
    expect(within(dialog).getByRole("region", { name: "Lost with the worktree" }).textContent).toContain("README.md");
    expect(reads(calls)).toHaveLength(1);
  });

  describe("for a workspace on another machine", () => {
    it("reads no changes, says the owner lists what would be lost, and still lists what stops", async () => {
      const calls = stub(() => json(200, changes));
      const dialog = await showElsewhere();

      const kept = within(dialog).getByRole("region", { name: "Kept on fix-login" });
      expect(kept.textContent).toBe("Kept on fix-loginIts changes can't be read from here. attic lists any work that would be lost before it archives.");
      expect(within(dialog).queryByRole("region", { name: "Lost with the worktree" })).toBeNull();
      const stops = within(dialog).getByRole("region", { name: "Stops" });
      expect(within(stops).getAllByRole("listitem").map((li) => li.textContent)).toEqual([
        expect.stringMatching(/opus.*fix the login bug.*Working.*mid-turn/),
        expect.stringMatching(/sonnet.*Ready/),
        expect.stringMatching(/shell.*Ended/),
      ]);
      await new Promise((resolve) => setTimeout(resolve, 20));
      expect(reads(calls)).toEqual([]);
      expect(posts(calls)).toEqual([]);
    });

    it("archives without force, closes and says what it kept on 204", async () => {
      const onArchived = vi.fn();
      const onClose = vi.fn();
      const calls = stub(() => json(200, changes), new Response(null, { status: 204 }));
      const dialog = await showElsewhere(onClose, onArchived);

      fireEvent.click(within(dialog).getByRole("button", { name: "Archive" }));

      await waitFor(() => expect(onArchived).toHaveBeenCalledWith("fix login", "fix-login"));
      expect(onClose).toHaveBeenCalled();
      expect(posts(calls)).toEqual([{ url: "/api/workspaces/w1/archive", method: "POST", body: { force: false } }]);
      expect(reads(calls)).toEqual([]);
    });

    it("lists what the owner holds the archive up for on 409 and archives anyway with force", async () => {
      const onArchived = vi.fn();
      const calls = stub(
        () => json(200, changes),
        json(409, { error: "workspace has unsaved work", uncommitted: ["src/a.rs"], unpushed: ["abc123 wip"], submodules: ["vendor/x"] }),
        new Response(null, { status: 204 }),
      );
      const dialog = await showElsewhere(vi.fn(), onArchived);

      fireEvent.click(within(dialog).getByRole("button", { name: "Archive" }));
      const held = await within(dialog).findByRole("region", { name: "Archiving now would lose this work" });
      expect(held.textContent).toContain("src/a.rs");
      expect(held.textContent).toContain("abc123 wip");
      expect(held.textContent).toContain("vendor/x");

      fireEvent.click(within(dialog).getByRole("button", { name: "Archive anyway" }));

      await waitFor(() => expect(onArchived).toHaveBeenCalled());
      expect(posts(calls).map((c) => c.body)).toEqual([{ force: false }, { force: true }]);
      expect(reads(calls)).toEqual([]);
    });

    it("stays open and says why on any other refusal", async () => {
      const onClose = vi.fn();
      stub(() => json(200, changes), json(503, { error: "attic is offline" }));
      const dialog = await showElsewhere(onClose);

      fireEvent.click(within(dialog).getByRole("button", { name: "Archive" }));

      expect((await within(dialog).findByRole("alert")).textContent).toBe("Could not archive fix login: attic is offline.");
      expect(onClose).not.toHaveBeenCalled();
      expect(screen.getByRole("dialog", { name: "Archive fix login?" })).toBeDefined();
    });
  });

  it("sends nothing on Cancel", async () => {
    const onClose = vi.fn();
    const calls = stub(() => json(200, changes));
    const dialog = await show(onClose);
    fireEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
    expect(onClose).toHaveBeenCalled();
    expect(posts(calls)).toEqual([]);
  });
});
