import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { machineQuery, type Briefs, type Changes, type Mark } from "./api";
import { fleet } from "./fleet";
import { JumpPalette } from "./JumpPalette";
import { ME, OTHER, session, settings, terminal, workspace } from "./test-fixtures";
import { json, renderApp } from "./test-utils";

const records = [
  workspace(ME, "w1", "fix login"),
  terminal(ME, "t1", "w1"),
  session(ME, "s1", "w1", "t1", "idle"),
  terminal(ME, "t2", "w1", "exited", { exit_code: 1 }),
  workspace(ME, "w2", "tidy docs"),
  terminal(ME, "t3", "w2"),
  session(ME, "s3", "w2", "t3", "working"),
  workspace(OTHER, "w9", "far away"),
  terminal(OTHER, "t9", "w9"),
  session(OTHER, "s9", "w9", "t9", "idle"),
];
const briefs: Briefs = {
  s1: { title: "Fix the login redirect", now: null, said: "Fixed it.\nThe redirect now keeps the query string." },
  s3: { title: "Tidy the docs", now: { kind: "tool", tool: "Read", text: "README.md" } },
};
const machines = fleet(
  records,
  settings({ machines: [{ id: ME, name: "studio", address: "a" }, { id: OTHER, name: "laptop", address: "b" }] }),
  briefs,
);
const now = Date.now();
const trail: Mark[] = [
  { kind: "prompt", text: "Fix the login redirect", at: now - 60_000 },
  { kind: "tool", tool: "Bash", text: "$ npm test", at: now - 50_000 },
  { kind: "tool", tool: "Edit", text: "src/login.ts", at: now - 40_000 },
  { kind: "done", text: "", at: now - 30_000 },
  // Claude Code's reminder after the turn ended: not a turn of its own.
  { kind: "ask", text: "Claude is waiting for your input", at: now - 20_000 },
];
const changes: Changes = {
  base: "main",
  commits: [{ hash: "abc1234", subject: "fix: keep the query string on redirect" }],
  files: [{ path: "src/login.ts", from: null, status: "modified", added: 3, removed: 1, uncommitted: false, patch: null }],
};

let urls: string[];
let wide: boolean;

beforeEach(() => {
  urls = [];
  wide = true;
  vi.stubGlobal("matchMedia", (query: string) => ({
    matches: query === "(min-width: 768px)" && wide,
    addEventListener: () => {},
    removeEventListener: () => {},
  }));
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      urls.push(url);
      if (url === "/api/sessions/s1/trail") return json(200, { steps: trail });
      if (url === "/api/sessions/s3/trail") return json(200, { steps: [] });
      if (url.endsWith("/changes")) return json(200, changes);
      throw new Error(`unexpected ${url}`);
    }),
  );
});

afterEach(() => {
  vi.unstubAllGlobals();
});

async function open() {
  const view = await renderApp(<JumpPalette machines={machines} open onOpenChange={() => {}} onCreate={() => {}} />);
  view.client.setQueryData(machineQuery.queryKey, ME);
  const dialog = await screen.findByRole("dialog", { name: "Jump to" });
  return { dialog, input: within(dialog).getByRole("combobox") };
}

describe("the jump palette's preview", () => {
  it("shows the highlighted session's state, what it was asked, what Claude said, its last turn and its workspace's changes", async () => {
    const { dialog } = await open();
    const peek = within(dialog).getByRole("region", { name: "Preview of fix login / opus" });
    expect(within(peek).getByText("Idle")).toBeDefined();
    expect(within(peek).getByText("“Fix the login redirect”")).toBeDefined();
    expect(within(peek).getByText(/The redirect now keeps the query string/)).toBeDefined();
    expect(await within(peek).findByText("1 command · 1 edit")).toBeDefined();
    expect(within(peek).getByText("npm test")).toBeDefined();
    expect(within(peek).getByText("Last turn")).toBeDefined();
    expect(within(peek).queryByText("Claude is waiting for your input")).toBeNull();
    expect(await within(peek).findByText("+3")).toBeDefined();
    expect(within(peek).getByText("fix: keep the query string on redirect")).toBeDefined();
    expect(urls).toEqual(expect.arrayContaining(["/api/sessions/s1/trail", "/api/workspaces/w1/changes"]));
  });

  it("follows the highlight, and reads only the row it rests on", async () => {
    const { dialog, input } = await open();
    await within(dialog).findByText("+3");
    urls.length = 0;
    fireEvent.keyDown(input, { key: "ArrowDown" });
    fireEvent.keyDown(input, { key: "ArrowDown" });
    const peek = within(dialog).getByRole("region", { name: "Preview of tidy docs / opus" });
    expect(within(peek).getByText("Working")).toBeDefined();
    expect(within(peek).getByText("README.md")).toBeDefined();
    await waitFor(() => expect(urls).toContain("/api/sessions/s3/trail"));
    // The shell passed over on the way was never read for.
    expect(urls.filter((u) => u.includes("w1") || u.includes("s1"))).toEqual([]);
  });

  it("says a shell has no steps, and shows how it ended", async () => {
    const { dialog, input } = await open();
    fireEvent.keyDown(input, { key: "ArrowDown" });
    const peek = within(dialog).getByRole("region", { name: "Preview of fix login / shell" });
    expect(within(peek).getByText("Exited 1")).toBeDefined();
    expect(within(peek).getByText(/A shell has no steps/)).toBeDefined();
  });

  it("asks another machine for nothing, and says where it runs", async () => {
    const { dialog, input } = await open();
    fireEvent.change(input, { target: { value: "far away" } });
    const peek = within(dialog).getByRole("region", { name: "Preview of far away / opus" });
    expect(within(peek).getByText(/runs on laptop/)).toBeDefined();
    await new Promise((r) => setTimeout(r, 400));
    expect(urls.filter((u) => u.includes("w9") || u.includes("s9"))).toEqual([]);
  });

  it("previews a new shell's workspace with its terminals", async () => {
    const { dialog, input } = await open();
    fireEvent.change(input, { target: { value: "new shell tidy" } });
    const peek = within(dialog).getByRole("region", { name: "Preview of tidy docs" });
    expect(within(peek).getByText("Working")).toBeDefined();
    expect(within(peek).getByText(/Starts a shell in tidy docs/)).toBeDefined();
  });

  it("has no preview on a narrow screen, and reads nothing for one", async () => {
    wide = false;
    const { dialog } = await open();
    await new Promise((r) => setTimeout(r, 400));
    expect(within(dialog).queryByRole("region", { name: /Preview/ })).toBeNull();
    expect(urls.filter((u) => u.includes("/trail") || u.includes("/changes"))).toEqual([]);
  });
});
