import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
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
