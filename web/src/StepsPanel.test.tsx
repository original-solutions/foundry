import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Mark } from "./api";
import { RETRY_EVERY } from "./poll";
import { STEPS_EVERY, StepsPanel, type StepsOf } from "./StepsPanel";
import { json, mockFetch, renderApp } from "./test-utils";

afterEach(() => vi.unstubAllGlobals());

const at = Date.now() - 60_000;
const trail: Mark[] = [
  { kind: "prompt", text: "fix the login bug", at },
  { kind: "tool", tool: "Read", text: "Read src/login.ts", at: at + 1000 },
  { kind: "tool", tool: "Bash", text: "$ npm test", at: at + 2000 },
  { kind: "done", text: "", at: at + 41_000 },
  { kind: "prompt", text: "now add a test", at: at + 50_000 },
  { kind: "tool", tool: "Edit", text: "Edit src/login.test.ts", at: at + 51_000 },
  { kind: "ask", text: "Claude needs your permission to use Bash", at: at + 52_000 },
];

const session: StepsOf = { kind: "session", id: "s1", name: "opus", glyph: "waiting", label: "Needs you" };

async function opened(of: StepsOf, ...answers: Response[]) {
  const onClose = vi.fn();
  const calls = mockFetch(...answers);
  await renderApp(<StepsPanel of={of} onClose={onClose} />);
  return { calls, onClose, panel: screen.getByRole("complementary", { name: "Steps" }) };
}

describe("StepsPanel", () => {
  it("reads the session's trail and shows its newest turn step by step", async () => {
    const { calls, panel } = await opened(session, json(200, { steps: trail }));
    const turn = await within(panel).findByRole("listitem", { name: /now add a test/ });
    expect(calls[0]).toEqual({ url: "/api/sessions/s1/trail", method: "GET", body: undefined });
    const steps = within(turn).getAllByRole("listitem").map((li) => li.textContent);
    expect(steps[0]).toContain("Edit src/login.test.ts");
    expect(steps[1]).toContain("Claude needs your permission to use Bash");
    expect(turn.textContent).toContain("Needs you");
  });

  it("folds an earlier turn to its prompt, what it did and how long it took, and opens it on a click", async () => {
    const { panel } = await opened(session, json(200, { steps: trail }));
    const earlier = await within(panel).findByRole("listitem", { name: /fix the login bug/ });
    expect(earlier.textContent).toContain("1 read · 1 command");
    expect(earlier.textContent).toContain("41s");
    expect(within(earlier).queryByText("npm test")).toBeNull();
    fireEvent.click(within(earlier).getByRole("button", { expanded: false }));
    expect(within(earlier).getByText("npm test")).toBeTruthy();
    expect(earlier.textContent).toMatch(/finished in 41s/i);
  });

  it("says when a session has taken no steps yet", async () => {
    const { panel } = await opened({ ...session, glyph: "ready", label: "Ready" }, json(200, { steps: [] }));
    await waitFor(() => expect(panel.textContent).toMatch(/no steps yet/i));
  });

  it("explains a session another machine runs instead of showing an error code", async () => {
    const { panel } = await opened(session, json(404, { error: "no session here" }));
    await waitFor(() => expect(panel.textContent).toMatch(/machine that runs it/i));
  });

  it("asks nothing for a shell, which has no hooks", async () => {
    const { calls, panel } = await opened({ kind: "shell", name: "shell" });
    expect(panel.textContent).toMatch(/shell has no hooks/i);
    expect(calls).toEqual([]);
  });

  it("closes from its button", async () => {
    const { onClose, panel } = await opened(session, json(200, { steps: [] }));
    fireEvent.click(within(panel).getByRole("button", { name: "Close steps" }));
    expect(onClose).toHaveBeenCalled();
  });
});

describe("StepsPanel by file", () => {
  afterEach(() => localStorage.clear());

  const changes = {
    base: "main",
    commits: [],
    files: [{ path: "src/login.test.ts", from: null, status: "added", added: 12, removed: 0, uncommitted: true, patch: "" }],
  };
  const inWorkspace: StepsOf = { ...session, workspace: "w1" };

  async function byFile(onFile = vi.fn()) {
    const calls = mockFetch(json(200, { steps: trail }), json(200, changes));
    await renderApp(<StepsPanel of={inWorkspace} onClose={() => {}} onFile={onFile} />);
    const panel = screen.getByRole("complementary", { name: "Steps" });
    fireEvent.click(within(panel).getByRole("button", { name: "Files" }));
    return { calls, panel, onFile };
  }

  it("turns the trail into the files the session touched, with what it did to each, and the commands it ran", async () => {
    const { calls, panel } = await byFile();
    const files = await within(panel).findByRole("list", { name: "Files" });
    const rows = within(files).getAllByRole("listitem").map((li) => li.textContent);
    expect(rows[0]).toContain("login.test.ts");
    expect(rows[0]).toContain("1 edit");
    expect(rows[1]).toContain("login.ts");
    expect(rows[1]).toContain("1 read");
    const commands = within(panel).getByRole("list", { name: "Commands" });
    expect(commands.textContent).toContain("npm test");
    expect(within(panel).queryByText(/needs your permission/)).toBeNull();
    await waitFor(() => expect(calls.map((c) => c.url)).toContain("/api/workspaces/w1/changes"));
  });

  it("shows a changed file's lines from the workspace's changes, and opens its diff", async () => {
    const { panel, onFile } = await byFile();
    const open = await within(panel).findByRole("button", { name: /src\/login\.test\.ts/ });
    expect(open.textContent).toContain("+12");
    fireEvent.click(open);
    expect(onFile).toHaveBeenCalledWith("src/login.test.ts");
    // A file only read, and not in the changes, has no diff to open.
    expect(within(panel).queryByRole("button", { name: /src\/login\.ts/ })).toBeNull();
  });

  it("says when an edited file holds no change any more, and offers no diff for it", async () => {
    mockFetch(json(200, { steps: trail }), json(200, { ...changes, files: [] }));
    await renderApp(<StepsPanel of={inWorkspace} onClose={() => {}} onFile={() => {}} />);
    fireEvent.click(screen.getByRole("button", { name: "Files" }));
    const files = await screen.findByRole("list", { name: "Files" });
    await waitFor(() => expect(within(files).getAllByRole("listitem")[0].textContent).toContain("no change now"));
    expect(within(files).queryByRole("button")).toBeNull();
  });

  it("keeps the choice of turns or files for the next session", async () => {
    await byFile();
    expect(screen.getByRole("button", { name: "Files" }).getAttribute("aria-pressed")).toBe("true");
    cleanup();
    mockFetch(json(200, { steps: trail }), json(200, changes));
    await renderApp(<StepsPanel of={inWorkspace} onClose={() => {}} />);
    expect(await screen.findByRole("list", { name: "Files" })).toBeDefined();
    fireEvent.click(screen.getByRole("button", { name: "Turns" }));
    expect(await screen.findByRole("listitem", { name: /now add a test/ })).toBeDefined();
  });

  it("opens an edit's diff from its step in a turn", async () => {
    const onFile = vi.fn();
    mockFetch(json(200, { steps: trail }));
    await renderApp(<StepsPanel of={inWorkspace} onClose={() => {}} onFile={onFile} />);
    fireEvent.click(await screen.findByRole("button", { name: /Edit src\/login\.test\.ts/ }));
    expect(onFile).toHaveBeenCalledWith("src/login.test.ts");
    // A read is not a change: it opens nothing.
    fireEvent.click(within(screen.getByRole("listitem", { name: /fix the login bug/ })).getByRole("button", { expanded: false }));
    expect(screen.queryByRole("button", { name: /Read src\/login\.ts/ })).toBeNull();
  });

  it("offers no files for a shell", async () => {
    await opened({ kind: "shell", name: "shell" });
    expect(screen.queryByRole("button", { name: "Files" })).toBeNull();
  });
});

describe("StepsPanel reading again", () => {
  let answer: () => Response;
  let reads = 0;

  /** The panel for `first`, with a button that moves the session to `then`, as a hook would. */
  function Moving({ first, then }: { first: StepsOf; then: StepsOf }) {
    const [of, setOf] = useState(first);
    return (
      <>
        <button type="button" onClick={() => setOf(then)}>
          change state
        </button>
        <StepsPanel of={of} onClose={() => {}} />
      </>
    );
  }

  async function watch(first: StepsOf, then: StepsOf = first) {
    reads = 0;
    vi.useFakeTimers({ shouldAdvanceTime: true });
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        reads += 1;
        return answer();
      }),
    );
    const view = await renderApp(<Moving first={first} then={then} />);
    await waitFor(() => expect(reads).toBe(1));
    return view;
  }

  const wait = (ms: number) => act(() => vi.advanceTimersByTimeAsync(ms));
  const working: StepsOf = { ...session, glyph: "working", label: "Working" };

  afterEach(() => vi.useRealTimers());

  it("reads a working session's trail every STEPS_EVERY", async () => {
    answer = () => json(200, { steps: trail });
    await watch(working);
    await wait(5 * STEPS_EVERY + 100);
    expect(reads).toBe(6);
  });

  it("does not read a waiting or stuck session on a timer, only when its state changes, under one entry", async () => {
    answer = () => json(200, { steps: trail });
    const stuck: StepsOf = { ...session, glyph: "stuck", label: "Stuck" };
    const { client } = await watch(session, stuck);
    await wait(60_000);
    expect(reads).toBe(1);
    fireEvent.click(screen.getByRole("button", { name: "change state" }));
    await waitFor(() => expect(reads).toBe(2));
    await wait(60_000);
    expect(reads).toBe(2);
    expect(client.getQueryCache().findAll({ queryKey: ["trail"] })).toHaveLength(1);
  });

  it("keeps reading a working session after a failed read, at the back-off", async () => {
    answer = () => json(500, { error: "daemon hiccup" });
    await watch(working);
    await wait(STEPS_EVERY + 100);
    expect(reads).toBe(1);
    answer = () => json(200, { steps: trail });
    await wait(RETRY_EVERY - STEPS_EVERY);
    expect(reads).toBe(2);
    await waitFor(() => expect(screen.getByRole("listitem", { name: /now add a test/ })).toBeDefined());
  });

  it("stops reading a session the daemon does not have", async () => {
    answer = () => json(404, { error: "no session here" });
    await watch(working);
    await wait(60_000);
    expect(reads).toBe(1);
  });
});
