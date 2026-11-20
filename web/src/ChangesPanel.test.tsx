import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Changes } from "./api";
import { ChangesPanel } from "./ChangesPanel";
import { loadViewed, saveViewed, seen } from "./review";
import { json, mockFetch, renderApp } from "./test-utils";

afterEach(() => {
  vi.unstubAllGlobals();
  localStorage.clear();
});

const target = { id: "w1", name: "fix login", branch: "fix-login" };

const changes: Changes = {
  base: "main",
  commits: ["abc1234 add login form"],
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

async function opened(answer: Response) {
  const calls = mockFetch(answer);
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
    expect(panel.textContent).toContain("abc1234");
    expect(panel.textContent).toContain("add login form");
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
    expect(alert.textContent).toContain("not a live workspace of this machine");
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
});
