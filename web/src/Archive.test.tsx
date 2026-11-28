import { cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ApiError, type Blockers, type Changes } from "./api";
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

/** Answers the changes read with `read`; any other request is unexpected. Gives back every request. */
function stub(read: () => Response | Promise<Response>): Call[] {
  const calls: Call[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init: RequestInit = {}) => {
      calls.push({ url, method: init.method ?? "GET", body: init.body ? JSON.parse(String(init.body)) : undefined });
      if (url === "/api/workspaces/w1/changes") return read();
      throw new Error("unexpected request");
    }),
  );
  return calls;
}

const posts = (calls: Call[]) => calls.filter((c) => c.method === "POST");

const studio = { id: ME, name: "studio" };
const attic = { id: OTHER, name: "attic" };

interface Shown {
  onClose?: () => void;
  onArchive?: (force: boolean) => void;
  machine?: typeof studio;
  /** The machine serving the UI; not known yet when null. */
  here?: string | null;
  held?: Blockers;
  failed?: unknown;
}

/** Shows the dialog for "fix login" on `machine` (studio), from the UI `here` (studio) serves. */
async function show({ onClose = vi.fn(), onArchive = vi.fn(), machine = studio, here = ME, held, failed }: Shown = {}) {
  await renderApp(<ArchiveDialog workspace={fixLogin()} machine={machine} here={here ?? undefined} held={held} failed={failed} onClose={onClose} onArchive={onArchive} />);
  return screen.findByRole("dialog", { name: "Archive fix login?" });
}

/** The same dialog for "fix login" on another machine, attic. */
const showElsewhere = (shown: Shown = {}) => show({ ...shown, machine: attic, here: ME });

const unsaved: Blockers = { uncommitted: ["src/a.rs"], unpushed: ["abc123 wip"], submodules: ["vendor/x"], detached: false };
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
    await renderApp(<ArchiveDialog workspace={quiet} machine={studio} here={ME} onClose={vi.fn()} onArchive={vi.fn()} />);
    dialog = await screen.findByRole("dialog", { name: "Archive fix login?" });
    expect(await within(dialog).findByText("Every change is committed.")).toBeDefined();
    expect(within(dialog).getByRole("button", { name: "Archive" }).className).toBe(primaryButton);
  });

  it("still archives when its changes cannot be read, and says they could not be", async () => {
    const onArchive = vi.fn();
    stub(() => Promise.reject(new TypeError("Failed to fetch")));
    const dialog = await show({ onArchive });
    expect(await within(dialog).findByText(/can.t be read from here/)).toBeDefined();
    fireEvent.click(within(dialog).getByRole("button", { name: "Archive" }));
    expect(onArchive).toHaveBeenCalledWith(false);
  });

  it("says in what stays on the branch why the daemon could not read its changes", async () => {
    const why = "workspace w1's folder is no longer a git worktree of its project; archive it to remove the folder";
    stub(() => json(404, { error: why }));
    const dialog = await show();
    const kept = within(dialog).getByRole("region", { name: "Kept on fix-login" });
    await waitFor(() => expect(kept.textContent).toContain(why));
    expect(kept.textContent).not.toMatch(/can.t be read from here/);
  });

  it("says the daemon's reason in what stays on the branch for any refused read, not only a 404", async () => {
    stub(() => json(500, { error: "git broke" }));
    const dialog = await show();
    const kept = within(dialog).getByRole("region", { name: "Kept on fix-login" });
    await waitFor(() => expect(kept.textContent).toContain("git broke."));
    expect(kept.textContent).not.toMatch(/can.t be read from here/);
  });

  it("asks for an archive without force once confirmed, and sends nothing itself", async () => {
    const onArchive = vi.fn();
    const calls = stub(() => json(200, changes));
    const dialog = await show({ onArchive });

    fireEvent.click(within(dialog).getByRole("button", { name: "Archive" }));

    expect(onArchive).toHaveBeenCalledWith(false);
    expect(posts(calls)).toEqual([]);
  });

  it("says why it cannot check a workspace whose folder is no longer a git worktree, and still lists its unpushed commits", async () => {
    const onArchive = vi.fn();
    stub(() => json(200, changes));
    const dialog = await show({ onArchive, held: { uncommitted: [], unpushed: ["abc123 wip"], submodules: [], detached: true } });

    const held = within(dialog).getByRole("region", { name: "Archiving now would lose this work" });
    expect(held.textContent).toContain("Its folder is no longer a git worktree, so what is in it can't be checked. Archive anyway deletes the folder; branch fix-login stays.");
    expect(within(held).getByText("fix-login").tagName).toBe("CODE");
    expect(held.textContent).toContain("abc123 wip");

    fireEvent.click(within(dialog).getByRole("button", { name: "Archive anyway" }));
    expect(onArchive).toHaveBeenCalledWith(true);
  });

  it("lists what held the archive up and asks to archive anyway with force", async () => {
    const onArchive = vi.fn();
    stub(() => json(200, changes));
    const dialog = await show({ onArchive, held: unsaved });

    const held = within(dialog).getByRole("region", { name: "Archiving now would lose this work" });
    expect(held.textContent).toContain("src/a.rs");
    expect(held.textContent).toContain("abc123 wip");
    expect(held.textContent).toContain("vendor/x");
    expect(held.textContent).not.toMatch(/no longer a git worktree/);
    expect(within(dialog).queryByRole("button", { name: "Archive" })).toBeNull();
    expect(within(dialog).getByRole("button", { name: "Archive anyway" }).className).toBe(dangerButton);

    fireEvent.click(within(dialog).getByRole("button", { name: "Archive anyway" }));

    expect(onArchive).toHaveBeenCalledWith(true);
  });

  it("says why the archive failed, and archives again from there", async () => {
    const onArchive = vi.fn();
    stub(() => json(200, changes));
    const dialog = await show({ onArchive, failed: new ApiError("disk on fire", 500) });

    expect(within(dialog).getByRole("alert").textContent).toBe("Could not archive fix login: disk on fire.");
    fireEvent.click(within(dialog).getByRole("button", { name: "Archive" }));
    expect(onArchive).toHaveBeenCalledWith(false);
  });

  it("reads and shows the changes while the machine serving the UI is not known yet", async () => {
    const calls = stub(() => json(200, changes));
    const dialog = await show({ machine: attic, here: null });
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

    it("lists what the owner held the archive up for and asks to archive anyway with force", async () => {
      const onArchive = vi.fn();
      const calls = stub(() => json(200, changes));
      const dialog = await showElsewhere({ onArchive, held: unsaved });

      const held = within(dialog).getByRole("region", { name: "Archiving now would lose this work" });
      expect(held.textContent).toContain("src/a.rs");
      expect(held.textContent).toContain("abc123 wip");
      expect(held.textContent).toContain("vendor/x");

      fireEvent.click(within(dialog).getByRole("button", { name: "Archive anyway" }));

      expect(onArchive).toHaveBeenCalledWith(true);
      expect(reads(calls)).toEqual([]);
    });
  });

  it("sends nothing on Cancel", async () => {
    const onClose = vi.fn();
    const onArchive = vi.fn();
    const calls = stub(() => json(200, changes));
    const dialog = await show({ onClose, onArchive });
    fireEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
    expect(onClose).toHaveBeenCalled();
    expect(onArchive).not.toHaveBeenCalled();
    expect(posts(calls)).toEqual([]);
  });
});
