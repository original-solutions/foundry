import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Changes, Mark } from "./api";
import { GLANCE_EVERY } from "./ChangesPanel";
import { StepsFiles } from "./StepsFiles";
import { json, renderApp } from "./test-utils";

const at = Date.now() - 60_000;
const trail: Mark[] = [
  { kind: "prompt", text: "fix the login bug", at },
  { kind: "tool", tool: "Edit", text: "Edit src/login.ts", at: at + 1000 },
];
const changes: Changes = {
  base: "main",
  commits: [],
  files: [{ path: "src/login.ts", from: null, status: "modified", added: 3, removed: 1, uncommitted: true, patch: "" }],
};

/** The files view of a session in `w1`, with a button that changes the session's state, as a hook would. */
function Moving() {
  const [label, setLabel] = useState("Working");
  return (
    <>
      <button type="button" onClick={() => setLabel((now) => (now === "Working" ? "Needs you" : "Working"))}>
        change state
      </button>
      <StepsFiles trail={trail} workspace="w1" label={label} />
    </>
  );
}

describe("StepsFiles reading the changes", () => {
  let answer: () => Response;
  let reads = 0;

  async function watch() {
    reads = 0;
    vi.useFakeTimers({ shouldAdvanceTime: true });
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) => {
        if (url !== "/api/workspaces/w1/changes") throw new Error(`unexpected request ${url}`);
        reads += 1;
        return answer();
      }),
    );
    await renderApp(<Moving />);
    await waitFor(() => expect(reads).toBe(1));
  }

  const wait = (ms: number) => act(() => vi.advanceTimersByTimeAsync(ms));
  const changeState = () => fireEvent.click(screen.getByRole("button", { name: "change state" }));

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("reads the changes every GLANCE_EVERY and again when the session's state changes", async () => {
    answer = () => json(200, changes);
    await watch();
    await wait(GLANCE_EVERY + 100);
    expect(reads).toBe(2);
    changeState();
    await waitFor(() => expect(reads).toBe(3));
  });

  it("stops reading a workspace the daemon answered 404, on the timer and when the session's state changes", async () => {
    answer = () => json(404, { error: "workspace w1 is not a live workspace of this machine" });
    await watch();
    await wait(5 * GLANCE_EVERY);
    expect(reads).toBe(1);
    changeState();
    await wait(GLANCE_EVERY);
    changeState();
    await wait(GLANCE_EVERY);
    expect(reads).toBe(1);
  });

  it("stops once a later read answers 404", async () => {
    answer = () => json(200, changes);
    await watch();
    answer = () => json(404, { error: "workspace w1 not found" });
    await wait(GLANCE_EVERY + 100);
    expect(reads).toBe(2);
    await wait(5 * GLANCE_EVERY);
    changeState();
    await wait(GLANCE_EVERY);
    expect(reads).toBe(2);
  });
});
