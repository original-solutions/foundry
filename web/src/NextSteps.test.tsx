import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Changes, Shipping } from "./api";
import type { Located } from "./fleet";
import { NextSteps, offersNext, StepChips } from "./NextSteps";
import { json, renderApp } from "./test-utils";

const dirty: Changes = {
  base: "main",
  commits: [],
  files: [{ path: "README.md", from: null, status: "modified", added: 2, removed: 0, uncommitted: true, patch: null }],
};
const quiet: Shipping = { branch: "say-hi", pushed: false, unpushed: 0, pr: null, unreachable: false };

/** Answers the workspace's two reads, and gives back the URLs read. */
function answer(changes: Changes, shipping: Shipping = quiet): string[] {
  const urls: string[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      urls.push(url);
      if (url === "/api/workspaces/w1/changes") return json(200, changes);
      if (url === "/api/workspaces/w1/pr") return json(200, shipping);
      throw new Error(`unexpected request ${url}`);
    }),
  );
  return urls;
}

afterEach(() => vi.unstubAllGlobals());

describe("NextSteps", () => {
  it("reads where the workspace stands and offers the next step, which goes to the session as a prompt", async () => {
    const urls = answer(dirty);
    const send = vi.fn(async () => {});
    await renderApp(<NextSteps workspace="w1" name="opus" send={send} look="header" />);

    const commit = await screen.findByRole("button", { name: "Commit 1 file" });
    expect(urls.sort()).toEqual(["/api/workspaces/w1/changes", "/api/workspaces/w1/pr"]);
    expect(commit.title).toMatch(/^Ask opus: Commit the uncommitted changes/);
    fireEvent.click(commit);
    await waitFor(() => expect(send).toHaveBeenCalledWith(expect.stringMatching(/^Commit the uncommitted changes/)));
    // Sent once: it stays down until the session starts working and the steps go away.
    expect((commit as HTMLButtonElement).disabled).toBe(true);
  });

  it("keeps a sent step down for as long as the steps stay the same, and lets one be pressed again once they change", () => {
    const push = { label: "Push and open a PR", prompt: "Push this branch and open a pull request." };
    const checks = { label: "Fix failing checks", prompt: "Fix the failing checks.", urgent: true };
    const send = vi.fn();
    vi.useFakeTimers();
    try {
      const { rerender } = render(<StepChips steps={[push]} name="opus" send={send} look="header" />);
      fireEvent.click(screen.getByRole("button", { name: "Push and open a PR" }));
      // A session whose hooks are down still reads as idle: a step that reaches GitHub is not offered again.
      act(() => vi.advanceTimersByTime(10 * 60_000));
      rerender(<StepChips steps={[{ ...push }]} name="opus" send={send} look="header" />);
      expect((screen.getByRole("button", { name: "Push and open a PR" }) as HTMLButtonElement).disabled).toBe(true);
      expect(send).toHaveBeenCalledTimes(1);

      rerender(<StepChips steps={[checks]} name="opus" send={send} look="header" />);
      expect((screen.getByRole("button", { name: "Fix failing checks" }) as HTMLButtonElement).disabled).toBe(false);
    } finally {
      vi.useRealTimers();
    }
  });

  it("says a step did not reach the session, and lets you try again", async () => {
    answer(dirty);
    const send = vi.fn().mockRejectedValueOnce(new Error("closed")).mockResolvedValueOnce(undefined);
    await renderApp(<NextSteps workspace="w1" name="opus" send={send} look="keys" />);

    fireEvent.click(await screen.findByRole("button", { name: "Commit 1 file" }));
    await screen.findByText("Not sent");
    fireEvent.click(screen.getByRole("button", { name: "Commit 1 file" }));
    await waitFor(() => expect(send).toHaveBeenCalledTimes(2));
  });

  it("shows nothing when there is nothing to do next", async () => {
    const urls = answer({ base: "main", commits: [], files: [] });
    await renderApp(<NextSteps workspace="w1" name="opus" send={() => {}} look="header" />);
    await waitFor(() => expect(urls).toHaveLength(2));
    expect(screen.queryByRole("group")).toBeNull();
  });
});

describe("offersNext", () => {
  const machine = { id: "machine-1", name: "mac", listed: true, status: "online" as const, sample: null, workspaces: [] };
  const workspace = { id: "w1", name: "say hi", branch: "say-hi", base: "main", project: "p", projectId: "p", sessions: [], shells: [] };
  const at = (over: Partial<Located>): Located => ({ machine, workspace, session: "s1", name: "opus", label: "Ready", glyph: "ready", ...over });

  it("is for an idle session of this machine only", () => {
    expect(offersNext(at({}), "machine-1")).toBe(true);
    expect(offersNext(at({ glyph: "working" }), "machine-1")).toBe(false);
    expect(offersNext(at({ glyph: "waiting" }), "machine-1")).toBe(false);
    expect(offersNext(at({ session: undefined, name: "shell" }), "machine-1")).toBe(false);
    expect(offersNext(at({}), "machine-2")).toBe(false);
    expect(offersNext(at({}), undefined)).toBe(false);
    expect(offersNext(undefined, "machine-1")).toBe(false);
  });
});
