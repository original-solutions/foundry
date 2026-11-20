import { describe, expect, it } from "vitest";
import type { Changes, FileChange, PullRequest, Shipping } from "./api";
import { nextSteps } from "./next";

const file = (over: Partial<FileChange> = {}): FileChange => ({
  path: "README.md",
  from: null,
  status: "modified",
  added: 1,
  removed: 0,
  uncommitted: false,
  patch: null,
  ...over,
});
const changes = (over: Partial<Changes> = {}): Changes => ({ base: "main", commits: [], files: [], ...over });
const committed = changes({ commits: [{ hash: "abc1234", subject: "docs: say hi" }], files: [file()] });
const at = (over: Partial<Shipping> = {}): Shipping => ({ branch: "say-hi", pushed: false, unpushed: 0, pr: null, unreachable: false, ...over });
const pr = (over: Partial<PullRequest> = {}): PullRequest => ({
  number: 12,
  title: "Say hi",
  url: "https://github.com/o/r/pull/12",
  state: "open",
  draft: false,
  checks: "none",
  review: "none",
  ...over,
});
const labels = (c: Changes, s?: Shipping) => nextSteps(c, s).map((step) => step.label);

describe("nextSteps", () => {
  it("offers nothing for a workspace that changed nothing", () => {
    expect(labels(changes(), at())).toEqual([]);
  });

  it("offers to commit uncommitted work first, and nothing that would leave it behind", () => {
    const c = changes({ commits: [{ hash: "abc1234", subject: "x" }], files: [file({ uncommitted: true }), file({ path: "b.ts", uncommitted: true }), file({ path: "c.ts" })] });
    expect(labels(c, at({ unpushed: 1 }))).toEqual(["Commit 2 files"]);
    expect(nextSteps(c, at())[0].prompt).toMatch(/^Commit the uncommitted changes/);
  });

  it("offers to commit while the branch's read is not in yet", () => {
    expect(labels(changes({ files: [file({ uncommitted: true })] }))).toEqual(["Commit 1 file"]);
  });

  it("offers to push and open a pull request once the work is committed and only here", () => {
    const [step] = nextSteps(committed, at({ unpushed: 1 }));
    expect(step.label).toBe("Push and open a PR");
    expect(step.prompt).toMatch(/open a pull request/);
  });

  it("offers to open a pull request for a pushed branch without one", () => {
    expect(labels(committed, at({ pushed: true }))).toEqual(["Open a PR"]);
  });

  it("never offers a pull request for a branch with no commits of its own", () => {
    expect(labels(changes(), at({ pushed: true }))).toEqual([]);
  });

  it("offers only a push when GitHub cannot say whether a pull request exists", () => {
    expect(labels(committed, at({ unpushed: 2, unreachable: true }))).toEqual(["Push 2 commits"]);
    expect(labels(committed, at({ pushed: true, unreachable: true }))).toEqual([]);
  });

  it("offers to fix failing checks and answer a review on an open pull request", () => {
    expect(labels(committed, at({ pushed: true, pr: pr({ checks: "failing", review: "changes_requested" }) }))).toEqual(["Fix failing checks", "Address the review"]);
  });

  it("offers to push commits a pull request does not have yet", () => {
    expect(labels(committed, at({ pushed: true, unpushed: 1, pr: pr({ checks: "passing" }) }))).toEqual(["Push 1 commit"]);
  });

  it("keeps the pull request's own asks beside a commit", () => {
    const c = changes({ commits: [{ hash: "abc1234", subject: "x" }], files: [file({ uncommitted: true })] });
    expect(labels(c, at({ pushed: true, unpushed: 1, pr: pr({ checks: "failing" }) }))).toEqual(["Commit 1 file", "Fix failing checks"]);
  });

  it("offers nothing once the pull request merged or closed", () => {
    expect(labels(committed, at({ pushed: true, pr: pr({ state: "merged" }) }))).toEqual([]);
    expect(labels(committed, at({ pushed: true, pr: pr({ state: "closed" }) }))).toEqual([]);
  });

  it("offers nothing while checks still run on a clean pull request", () => {
    expect(labels(committed, at({ pushed: true, pr: pr({ checks: "pending", review: "required" }) }))).toEqual([]);
  });
});
