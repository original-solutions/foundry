import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Mark } from "./api";
import { StepsPanel, type StepsOf } from "./StepsPanel";
import { json, mockFetch, renderApp } from "./test-utils";

afterEach(() => vi.unstubAllGlobals());

const at = Date.now() - 60_000;
const trail: Mark[] = [
  { kind: "prompt", text: "fix the login bug", at },
  { kind: "tool", text: "Read src/login.ts", at: at + 1000 },
  { kind: "tool", text: "$ npm test", at: at + 2000 },
  { kind: "done", text: "", at: at + 41_000 },
  { kind: "prompt", text: "now add a test", at: at + 50_000 },
  { kind: "tool", text: "Edit src/login.test.ts", at: at + 51_000 },
  { kind: "ask", text: "Claude needs your permission to use Bash", at: at + 52_000 },
];

const session: StepsOf = { kind: "session", id: "s1", name: "opus", glyph: "waiting", label: "Waiting for you" };

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
    expect(turn.textContent).toContain("Waiting for you");
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
    const { panel } = await opened({ ...session, glyph: "ready", label: "Idle" }, json(200, { steps: [] }));
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
