import { fireEvent, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { PullRequest, Shipping } from "./api";
import { ShipLine, shipLine } from "./Shipping";
import { json, renderApp } from "./test-utils";

/** Stubs `fetch` to answer each read with `answers` in order, and returns the URLs read. */
function answer(...answers: Shipping[]): string[] {
  const urls: string[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      urls.push(url);
      const next = answers.shift();
      if (!next) throw new Error("unexpected request");
      return json(200, next);
    }),
  );
  return urls;
}

const at = (over: Partial<Shipping> = {}): Shipping => ({ branch: "fix-login", pushed: false, unpushed: 0, pr: null, unreachable: false, ...over });
const pr = (over: Partial<PullRequest> = {}): PullRequest => ({
  number: 12,
  title: "Fix the login form",
  url: "https://github.com/o/r/pull/12",
  state: "open",
  draft: false,
  checks: "none",
  review: "none",
  ...over,
});
const words = (s: Shipping) => {
  const line = shipLine(s);
  return line && [line.lead, ...line.notes.map((n) => n.text)].join(" · ");
};

afterEach(() => vi.unstubAllGlobals());

describe("shipLine", () => {
  it("says nothing of a branch that was never pushed and has nothing to push", () => {
    expect(shipLine(at())).toBeUndefined();
  });

  it("says what is only on this machine before the branch reaches GitHub", () => {
    expect(words(at({ unpushed: 1 }))).toBe("Not pushed · 1 commit only here");
    expect(words(at({ unpushed: 3 }))).toBe("Not pushed · 3 commits only here");
  });

  it("says a pushed branch has no pull request, or that GitHub could not be asked", () => {
    expect(words(at({ pushed: true }))).toBe("Pushed · No PR yet");
    expect(words(at({ pushed: true, unpushed: 2 }))).toBe("Pushed · No PR yet · 2 commits not pushed");
    expect(words(at({ pushed: true, unreachable: true }))).toBe("Pushed · GitHub not reachable");
  });

  it("names an open pull request by its checks and review, worst first", () => {
    expect(words(at({ pushed: true, pr: pr({ checks: "failing", review: "required" }) }))).toBe("PR #12 · Checks failing · Review required");
    expect(words(at({ pushed: true, pr: pr({ checks: "pending", review: "changes_requested" }) }))).toBe("PR #12 · Checks running · Changes requested");
    expect(words(at({ pushed: true, pr: pr({ checks: "passing", review: "approved" }) }))).toBe("PR #12 · Checks pass · Approved");
    expect(words(at({ pushed: true, pr: pr({ draft: true }) }))).toBe("Draft PR #12");
    expect(words(at({ pushed: true, unpushed: 1, pr: pr() }))).toBe("PR #12 · 1 commit not pushed");
    expect(shipLine(at({ pushed: true, pr: pr({ checks: "failing" }) }))?.tone).toBe("bad");
    expect(shipLine(at({ pushed: true, pr: pr({ checks: "passing", review: "approved" }) }))?.tone).toBe("good");
  });

  it("offers to archive once the pull request merged, unless work came after it", () => {
    const merged = shipLine(at({ pushed: true, pr: pr({ state: "merged" }) }));
    expect(merged?.lead).toBe("PR #12 merged");
    expect(merged?.done).toBe(true);
    const after = shipLine(at({ pushed: true, unpushed: 1, pr: pr({ state: "merged" }) }));
    expect(words(at({ pushed: true, unpushed: 1, pr: pr({ state: "merged" }) }))).toBe("PR #12 merged · 1 commit not pushed");
    expect(after?.done).toBe(false);
    expect(words(at({ pushed: true, pr: pr({ state: "closed" }) }))).toBe("PR #12 closed");
  });
});

describe("ShipLine", () => {
  it("links the pull request to GitHub and archives a merged one in one click", async () => {
    answer(at({ pushed: true, pr: pr({ state: "merged" }) }));
    const archive = vi.fn();
    await renderApp(<ShipLine workspace="w1" name="fix login" enabled onArchive={archive} />);
    const link = await screen.findByRole("link", { name: /PR #12 merged/ });
    expect(link.getAttribute("href")).toBe("https://github.com/o/r/pull/12");
    expect(link.getAttribute("target")).toBe("_blank");
    expect(link.getAttribute("title")).toContain("Fix the login form");
    fireEvent.click(screen.getByRole("button", { name: "Archive fix login: its pull request merged" }));
    expect(archive).toHaveBeenCalled();
  });

  it("offers no archive where this machine cannot archive, and shows nothing until there is something to say", async () => {
    const calls = answer(at({ pushed: true, pr: pr({ state: "merged" }) }));
    const { unmount } = await renderApp(<ShipLine workspace="w1" name="fix login" enabled />);
    await screen.findByRole("link", { name: /PR #12 merged/ });
    expect(screen.queryByRole("button", { name: /Archive/ })).toBeNull();
    expect(calls).toEqual(["/api/workspaces/w1/pr"]);
    unmount();

    answer(at());
    const view = await renderApp(<ShipLine workspace="w2" name="tidy" enabled />);
    await waitFor(() => expect(view.container.textContent).toBe(""));
  });

  it("reads nothing while not enabled", async () => {
    const calls = answer();
    await renderApp(<ShipLine workspace="w1" name="fix login" enabled={false} />);
    expect(calls).toEqual([]);
  });
});
