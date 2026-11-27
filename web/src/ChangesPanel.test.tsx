import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Changes } from "./api";
import { ChangesPanel } from "./ChangesPanel";
import { loadViewed, saveViewed, seen } from "./review";
import { expectFieldLook, FakeSocket, json, mockFetch, renderApp } from "./test-utils";

afterEach(() => {
  vi.unstubAllGlobals();
  localStorage.clear();
});

const target = { id: "w1", name: "fix login", branch: "fix-login" };

const changes: Changes = {
  base: "main",
  commits: [{ hash: "abc1234", subject: "add login form" }],
  files: [
    {
      path: "src/login.ts",
      from: null,
      status: "modified",
      added: 2,
      removed: 1,
      uncommitted: true,
      patch: "@@ -4,2 +4,3 @@\n const a = 1;\n-const b = 2;\n+const b = 3;\n+const c = 4;\n",
    },
    { path: "logo.png", from: null, status: "added", added: null, removed: null, uncommitted: false, patch: null },
    { path: "docs/notes.md", from: null, status: "untracked", added: 1, removed: 0, uncommitted: true, patch: "@@ -0,0 +1,1 @@\n+hello\n" },
  ],
};

async function opened(...answers: Response[]) {
  const calls = mockFetch(...answers);
  await renderApp(<ChangesPanel workspace={target} onClose={() => {}} />);
  return { calls, panel: await screen.findByRole("dialog", { name: /changes in fix login/i }) };
}

describe("ChangesPanel", () => {
  it("reads the workspace's changes and sums them up against its base", async () => {
    const { calls, panel } = await opened(json(200, changes));
    await waitFor(() => expect(panel.textContent).toContain("+3"));
    expect(calls[0]).toEqual({ url: "/api/workspaces/w1/changes", method: "GET", body: undefined });
    expect(panel.textContent).toContain("fix-login");
    expect(panel.textContent).toContain("main");
    expect(panel.textContent).toContain("−1");
    expect(panel.textContent).toContain("3 files");
    expect(panel.textContent).toContain("2 uncommitted");
    const commits = within(panel).getByRole("region", { name: "Commits" });
    expect(within(commits).getByText("abc1234")).toBeDefined();
    expect(within(commits).getByText("add login form")).toBeDefined();
  });

  it("shows each file's diff with its lines numbered and marked added or removed", async () => {
    const { panel } = await opened(json(200, changes));
    const file = await within(panel).findByRole("region", { name: "src/login.ts" });
    const added = within(file).getAllByRole("row", { name: /^added/ });
    expect(added.map((r) => r.textContent)).toEqual(["5+const b = 3;", "6+const c = 4;"]);
    expect(within(file).getByRole("row", { name: /^removed/ }).textContent).toBe("5-const b = 2;");
    expect(file.textContent).toContain("Uncommitted");

    const binary = within(panel).getByRole("region", { name: "logo.png" });
    expect(binary.textContent).toMatch(/not shown/i);
    expect(within(panel).getByRole("region", { name: "docs/notes.md" }).textContent).toMatch(/new/i);
  });

  it("colours a diff as code in the file's language, and marks the words a changed line changed", async () => {
    const { panel } = await opened(json(200, changes));
    const file = await within(panel).findByRole("region", { name: "src/login.ts" });
    const removed = within(file).getByRole("row", { name: /^removed/ });
    const added = within(file).getAllByRole("row", { name: /^added/ });
    expect([...removed.querySelectorAll("[data-tone=keyword]")].map((e) => e.textContent)).toEqual(["const"]);
    expect([...removed.querySelectorAll("mark")].map((e) => e.textContent)).toEqual(["2"]);
    expect([...added[0].querySelectorAll("mark")].map((e) => e.textContent)).toEqual(["3"]);
    expect(added[1].querySelector("mark")).toBeNull();
    expect(added[1].querySelector("[data-tone=number]")?.textContent).toBe("4");
  });

  it("folds a file away and back", async () => {
    const { panel } = await opened(json(200, changes));
    const file = await within(panel).findByRole("region", { name: "src/login.ts" });
    const toggle = within(file).getByRole("button", { name: /src\/login\.ts/ });
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(within(file).queryAllByRole("row")).toHaveLength(0);
  });

  it("says so when nothing has changed since the base", async () => {
    const { panel } = await opened(json(200, { base: "main", commits: [], files: [] }));
    await waitFor(() => expect(panel.textContent).toMatch(/no changes since main/i));
  });

  it("shows why the changes could not be read", async () => {
    const { panel } = await opened(json(404, { error: "workspace w1 is not a live workspace of this machine" }));
    const alert = await within(panel).findByRole("alert");
    expect(alert.textContent).toBe("Could not read the changes: workspace w1 is not a live workspace of this machine.");
  });

  it("marks a file viewed: it folds, the count moves on, and this browser keeps it", async () => {
    const { panel } = await opened(json(200, changes));
    const file = await within(panel).findByRole("region", { name: "src/login.ts" });
    expect(panel.textContent).toContain("0 of 3 viewed");
    fireEvent.click(within(file).getByRole("checkbox", { name: "Viewed" }));
    expect(within(panel).getByRole("region", { name: "src/login.ts" }).querySelector("[aria-expanded]")?.getAttribute("aria-expanded")).toBe("false");
    expect(panel.textContent).toContain("1 of 3 viewed");
    expect(Object.keys(loadViewed("w1"))).toEqual(["src/login.ts"]);
  });

  it("opens a file again when it changed after you viewed it, and marks the lines new since then", async () => {
    const before = { ...changes.files[0], patch: "@@ -4,2 +4,2 @@\n const a = 1;\n-const b = 2;\n+const b = 3;\n" };
    saveViewed("w1", { "src/login.ts": seen(before) }, changes.files);
    const { panel } = await opened(json(200, changes));
    const file = await within(panel).findByRole("region", { name: "src/login.ts" });
    expect(file.textContent).toContain("Changed since viewed");
    expect(within(file).getByRole("checkbox", { name: "Viewed" })).toHaveProperty("checked", false);
    expect(within(file).getAllByRole("row", { name: /new since viewed/ }).map((r) => r.textContent)).toEqual(["6+const c = 4;"]);
    expect(panel.textContent).toContain("1 changed since viewed");
  });

  it("opens at a file named in a terminal: it is the current one and scrolled to", async () => {
    mockFetch(json(200, changes));
    const scrolled = vi.fn();
    Element.prototype.scrollIntoView = scrolled;
    try {
      await renderApp(<ChangesPanel workspace={target} file="docs/notes.md" onClose={() => {}} />);
      const panel = await screen.findByRole("dialog", { name: /changes in fix login/i });
      await within(panel).findByRole("region", { name: "docs/notes.md" });
      await waitFor(() => expect(panel.querySelector("section[aria-current='true']")?.getAttribute("aria-label")).toBe("docs/notes.md"));
      expect(scrolled).toHaveBeenCalledTimes(1);
      expect(scrolled.mock.contexts[0]).toBe(panel.querySelector("[data-path='docs/notes.md']"));
    } finally {
      delete (Element.prototype as Partial<Element>).scrollIntoView;
    }
  });

  it("walks the files from the keyboard: j and k move, v marks viewed and moves to the next one to view", async () => {
    const { panel } = await opened(json(200, changes));
    await within(panel).findByRole("region", { name: "src/login.ts" });
    const current = () => panel.querySelector("section[aria-current='true']")?.getAttribute("aria-label");
    fireEvent.keyDown(panel, { key: "j" });
    expect(current()).toBe("src/login.ts");
    fireEvent.keyDown(panel, { key: "j" });
    expect(current()).toBe("logo.png");
    fireEvent.keyDown(panel, { key: "k" });
    expect(current()).toBe("src/login.ts");
    fireEvent.keyDown(panel, { key: "v" });
    expect(panel.textContent).toContain("1 of 3 viewed");
    expect(current()).toBe("logo.png");
    fireEvent.keyDown(panel, { key: "v" });
    fireEvent.keyDown(panel, { key: "v" });
    expect(panel.textContent).toContain("All 3 viewed");
    fireEvent.keyDown(panel, { key: "j", metaKey: true });
    expect(current()).toBe("docs/notes.md");
  });

  describe("commit by commit", () => {
    const two: Changes = { ...changes, commits: [{ hash: "bbb2222", subject: "style the form" }, ...changes.commits] };
    const first: Changes = { base: "main", commits: [changes.commits[0]], files: [{ ...changes.files[0], uncommitted: false }] };
    const second: Changes = {
      base: "main",
      commits: [two.commits[0]],
      files: [{ path: "src/form.css", from: null, status: "added", added: 1, removed: 0, uncommitted: false, patch: "@@ -0,0 +1 @@\n+form {}\n" }],
    };

    it("lists the branch's commits oldest first, and a commit picked shows only what it changed, to read, not review", async () => {
      const { calls, panel } = await opened(json(200, two), json(200, first));
      const story = await within(panel).findByRole("region", { name: "Commits" });
      const steps = within(story).getAllByRole("button");
      expect(steps.map((b) => b.textContent)).toEqual([expect.stringContaining("All changes"), expect.stringContaining("add login form"), expect.stringContaining("style the form")]);
      expect(steps[0].getAttribute("aria-pressed")).toBe("true");

      fireEvent.click(steps[1]);
      const commit = await within(panel).findByRole("region", { name: "Commit 1 of 2" });
      expect(calls[1].url).toBe("/api/workspaces/w1/commits/abc1234");
      expect(commit.textContent).toContain("abc1234");
      expect(commit.textContent).toContain("add login form");
      const file = await within(panel).findByRole("region", { name: "src/login.ts" });
      expect(within(panel).queryByRole("region", { name: "logo.png" })).toBeNull();
      expect(within(file).queryByRole("checkbox", { name: "Viewed" })).toBeNull();
      expect(within(file).queryByRole("button", { name: /comment on/i })).toBeNull();
      expect(file.textContent).not.toContain("Uncommitted");
      expect(within(panel).getByRole("navigation", { name: "Changed files" }).textContent).toBe("Mlogin.tssrc");
      expect(panel.textContent).toContain("0 of 3 viewed");

      fireEvent.click(within(story).getByRole("button", { name: /all changes/i }));
      expect(await within(within(panel).getByRole("region", { name: "src/login.ts" })).findByRole("checkbox", { name: "Viewed" })).toBeDefined();
      expect(within(panel).getByRole("region", { name: "logo.png" })).toBeDefined();
    });

    it("steps through the commits from the keyboard: ] to the next, [ back, past the first to all changes", async () => {
      const { calls, panel } = await opened(json(200, two), json(200, first), json(200, second));
      await within(panel).findByRole("region", { name: "Commits" });
      fireEvent.keyDown(panel, { key: "]" });
      await within(panel).findByRole("region", { name: "Commit 1 of 2" });
      fireEvent.keyDown(panel, { key: "]" });
      await within(panel).findByRole("region", { name: "Commit 2 of 2" });
      expect(await within(panel).findByRole("region", { name: "src/form.css" })).toBeDefined();
      expect(calls.map((c) => c.url).slice(1)).toEqual(["/api/workspaces/w1/commits/abc1234", "/api/workspaces/w1/commits/bbb2222"]);
      fireEvent.keyDown(panel, { key: "]" });
      expect(within(panel).getByRole("region", { name: "Commit 2 of 2" })).toBeDefined();
      fireEvent.keyDown(panel, { key: "[" });
      await within(panel).findByRole("region", { name: "Commit 1 of 2" });
      expect(calls).toHaveLength(3);
      fireEvent.keyDown(panel, { key: "[" });
      await waitFor(() => expect(within(panel).queryByRole("region", { name: /^Commit \d/ })).toBeNull());
      expect(within(panel).getByRole("region", { name: "logo.png" })).toBeDefined();
    });

    it("says why a commit could not be read, and all changes stay a click away", async () => {
      const { panel } = await opened(json(200, two), json(404, { error: "commit abc1234 is not on this workspace's branch" }));
      const story = await within(panel).findByRole("region", { name: "Commits" });
      fireEvent.click(within(story).getByRole("button", { name: /add login form/ }));
      expect((await within(panel).findByRole("alert")).textContent).toBe("Could not read commit abc1234: commit abc1234 is not on this workspace's branch.");
      fireEvent.click(within(story).getByRole("button", { name: /all changes/i }));
      expect(await within(panel).findByRole("region", { name: "logo.png" })).toBeDefined();
    });
  });

  it("names a workspace on a commit by its commit, as the board does", async () => {
    mockFetch(json(200, changes));
    await renderApp(<ChangesPanel workspace={{ ...target, branch: "", base: "fedcba9876543210fedcba9876543210fedcba98" }} onClose={() => {}} />);
    const panel = await screen.findByRole("dialog", { name: /changes in fix login/i });
    expect(within(panel).getByText("fedcba9")).toBeDefined();
  });

  describe("comments for Claude", () => {
    const sessions = [
      { id: "s1", model: "opus", name: "opus 1", state: "idle" as const, terminal: "t1" },
      { id: "s2", model: "opus", name: "opus 2", state: "working" as const, terminal: "t2" },
    ];
    const typed = (socket: FakeSocket) => socket.sent.map((d) => (typeof d === "string" ? d : new TextDecoder().decode(d)));

    async function commented(over: { sessions?: typeof sessions } = {}) {
      FakeSocket.install();
      mockFetch(json(200, changes), json(200, changes));
      const view = await renderApp(<ChangesPanel workspace={{ ...target, sessions: over.sessions ?? sessions }} onClose={() => {}} />);
      const panel = await screen.findByRole("dialog", { name: /changes in fix login/i });
      const file = await within(panel).findByRole("region", { name: "src/login.ts" });
      fireEvent.click(within(file).getByRole("button", { name: "Comment on line 6" }));
      const box = within(file).getByRole("textbox", { name: "Comment on line 6" });
      expectFieldLook(box);
      fireEvent.change(box, { target: { value: "call it total" } });
      fireEvent.keyDown(box, { key: "Enter", metaKey: true });
      return { ...view, panel, file };
    }

    it("puts a comment under the line it was written on, and typing j or v in it moves nothing", async () => {
      FakeSocket.install();
      mockFetch(json(200, changes));
      await renderApp(<ChangesPanel workspace={{ ...target, sessions }} onClose={() => {}} />);
      const panel = await screen.findByRole("dialog", { name: /changes in fix login/i });
      const file = await within(panel).findByRole("region", { name: "src/login.ts" });
      fireEvent.click(within(file).getByRole("button", { name: "Comment on removed line 5" }));
      const box = within(file).getByRole("textbox", { name: "Comment on removed line 5" });
      fireEvent.keyDown(box, { key: "j" });
      fireEvent.keyDown(box, { key: "v" });
      expect(panel.querySelector("section[aria-current='true']")).toBeNull();
      expect(panel.textContent).toContain("0 of 3 viewed");
      fireEvent.change(box, { target: { value: "keep b at 2" } });
      fireEvent.click(within(file).getByRole("button", { name: "Add comment" }));
      const note = within(file).getByRole("row", { name: "Comment on removed line 5" });
      expect(note.textContent).toContain("keep b at 2");
      const rows = within(file).getAllByRole("row");
      expect(rows.indexOf(note)).toBe(rows.findIndex((r) => r.textContent === "5-const b = 2;") + 1);
      expect(within(panel).getByRole("button", { name: "Send 1 comment to opus 1" })).toBeDefined();
    });

    it("Escape in a comment box closes the box, not the sheet", async () => {
      FakeSocket.install();
      mockFetch(json(200, changes));
      await renderApp(<ChangesPanel workspace={{ ...target, sessions }} onClose={() => {}} />);
      const panel = await screen.findByRole("dialog", { name: /changes in fix login/i });
      const file = await within(panel).findByRole("region", { name: "src/login.ts" });
      fireEvent.click(within(file).getByRole("button", { name: "Comment on line 6" }));
      fireEvent.keyDown(within(file).getByRole("textbox"), { key: "Escape" });
      expect(within(file).queryByRole("textbox")).toBeNull();
      expect(screen.getByRole("dialog", { name: /changes in fix login/i })).toBeDefined();
    });

    it("sends every comment to the idle session as one message and presses Enter, then says so", async () => {
      const { panel, file } = await commented();
      expectFieldLook(within(panel).getByRole("combobox", { name: "to" }));
      fireEvent.click(within(panel).getByRole("button", { name: "Send 1 comment to opus 1" }));
      const socket = FakeSocket.last();
      expect(socket.url).toMatch(/\/api\/terminals\/t1$/);
      act(() => socket.open());
      expect(typed(socket)).toEqual([
        "\x1b[200~Review comments on your changes. Address each one, then say what you changed.\n\n1. src/login.ts:6\n   > const c = 4;\n   call it total\x1b[201~",
        "\r",
      ]);
      await waitFor(() => expect(within(panel).getByRole("status").textContent).toContain("Sent 1 comment to opus 1"));
      expect(within(file).queryByRole("row", { name: /^Comment on/ })).toBeNull();
    });

    it("after sending, shows the session at work on the comments for as long as it works", async () => {
      FakeSocket.install();
      mockFetch(json(200, changes));
      function Working() {
        const [state, setState] = useState<"idle" | "working">("idle");
        return (
          <>
            <button onClick={() => setState("working")}>Start the turn</button>
            <ChangesPanel workspace={{ ...target, sessions: [{ ...sessions[0], name: "opus", state }] }} onClose={() => {}} />
          </>
        );
      }
      await renderApp(<Working />);
      const file = await screen.findByRole("region", { name: "src/login.ts" });
      fireEvent.click(within(file).getByRole("button", { name: "Comment on line 6" }));
      fireEvent.change(within(file).getByRole("textbox"), { target: { value: "call it total" } });
      fireEvent.keyDown(within(file).getByRole("textbox"), { key: "Enter", metaKey: true });
      fireEvent.click(screen.getByRole("button", { name: "Send 1 comment to opus" }));
      act(() => FakeSocket.last().open());
      await screen.findByRole("status");
      fireEvent.click(screen.getByRole("button", { name: "Start the turn", hidden: true }));
      const status = screen.getByRole("status");
      expect(status.textContent).toContain("opus is working on it");
      expect(status.querySelector("[data-glyph='working']")).not.toBeNull();
    });

    it("keeps the comments and says so when the session cannot be reached", async () => {
      const { panel, file } = await commented();
      fireEvent.click(within(panel).getByRole("button", { name: "Send 1 comment to opus 1" }));
      act(() => FakeSocket.last().end(1011, "terminal gone"));
      await waitFor(() => expect(within(panel).getByRole("alert").textContent).toContain("Could not reach opus 1"));
      expect(within(file).getByRole("row", { name: "Comment on line 6" }).textContent).toContain("call it total");
    });

    it("says a live factory run's stuck session is quiet, as the board does", async () => {
      FakeSocket.install();
      mockFetch(json(200, changes), json(200, changes));
      const quiet = [{ ...sessions[1], name: "opus", state: "stuck" as const }];
      await renderApp(<ChangesPanel workspace={{ ...target, sessions: quiet, factory: { run: "o__r__162__1" } }} onClose={() => {}} />);
      const panel = await screen.findByRole("dialog", { name: /changes in fix login/i });
      const file = await within(panel).findByRole("region", { name: "src/login.ts" });
      fireEvent.click(within(file).getByRole("button", { name: "Comment on line 6" }));
      fireEvent.change(within(file).getByRole("textbox", { name: "Comment on line 6" }), { target: { value: "x" } });
      fireEvent.keyDown(within(file).getByRole("textbox", { name: "Comment on line 6" }), { key: "Enter", metaKey: true });
      expect(panel.textContent).toContain("opus is quiet 10+ min");
    });

    it("sends nothing while no session is idle, and says why", async () => {
      const { panel } = await commented({ sessions: [{ ...sessions[1], name: "opus" }] });
      const send = within(panel).getByRole("button", { name: "Send 1 comment to opus" });
      expect(send).toHaveProperty("disabled", true);
      expect(panel.textContent).toContain("opus is working");
      fireEvent.click(send);
      expect(FakeSocket.all).toHaveLength(0);
    });

    it("keeps unsent comments when the sheet closes and opens again", async () => {
      FakeSocket.install();
      mockFetch(json(200, changes), json(200, changes));
      function Reopen() {
        const [open, setOpen] = useState(true);
        return (
          <>
            <button onClick={() => setOpen(true)}>Open changes</button>
            <ChangesPanel workspace={open ? { ...target, sessions } : undefined} onClose={() => setOpen(false)} />
          </>
        );
      }
      await renderApp(<Reopen />);
      const file = await screen.findByRole("region", { name: "src/login.ts" });
      fireEvent.click(within(file).getByRole("button", { name: "Comment on line 6" }));
      fireEvent.change(within(file).getByRole("textbox"), { target: { value: "call it total" } });
      fireEvent.keyDown(within(file).getByRole("textbox"), { key: "Enter", metaKey: true });
      fireEvent.click(screen.getByRole("button", { name: "Close changes" }));
      await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
      fireEvent.click(screen.getByRole("button", { name: "Open changes" }));
      const again = await screen.findByRole("region", { name: "src/login.ts" });
      expect(within(again).getByRole("row", { name: "Comment on line 6" }).textContent).toContain("call it total");
    });

    it("deletes a comment, and edits one by clicking it", async () => {
      const { panel, file } = await commented();
      fireEvent.click(within(file).getByRole("button", { name: /call it total/ }));
      const box = within(file).getByRole("textbox", { name: "Comment on line 6" });
      expect((box as HTMLTextAreaElement).value).toBe("call it total");
      fireEvent.change(box, { target: { value: "call it sum" } });
      fireEvent.keyDown(box, { key: "Enter", ctrlKey: true });
      expect(within(file).getByRole("row", { name: "Comment on line 6" }).textContent).toContain("call it sum");
      fireEvent.click(within(file).getByRole("button", { name: "Delete comment on line 6" }));
      expect(within(file).queryByRole("row", { name: /^Comment on/ })).toBeNull();
      expect(within(panel).queryByRole("button", { name: /^Send/ })).toBeNull();
    });
  });
});
