import { describe, expect, it } from "vitest";
import { safeBranch, sameBranch } from "./branch";

describe("safeBranch", () => {
  // The cases the daemon's `safe_branch` is tested with, so the preview names the branch it makes.
  it.each([
    ["fix-login", "fix-login"],
    ["fix login", "fix-login"],
    ["Fix Login", "Fix-Login"],
    ["  fix   login  ", "fix-login"],
    ["café ☕ täst", "café-täst"],
    ["CAFÉ", "CAFÉ"],
    ["../../escape", "escape"],
    ["-rf", "rf"],
    ["--force", "force"],
    ["a..b", "a.b"],
    ["x.lock", "x-lock"],
    ["feature/login", "feature-login"],
    ["@{u}", "u"],
    [".hidden", "hidden"],
    ["trailing.", "trailing"],
    ["HEAD", "HEAD-workspace"],
    ["", "workspace"],
    ["...", "workspace"],
    ["-", "workspace"],
  ])("makes %j into %j", (name, branch) => {
    expect(safeBranch(name)).toBe(branch);
  });

  it("cuts a long name at 100 bytes, on a character", () => {
    expect(safeBranch("日".repeat(60))).toBe("日".repeat(33));
  });
});

describe("sameBranch", () => {
  it("compares as a disk that ignores case does", () => {
    expect(sameBranch("Fix-Login", "fix-login")).toBe(true);
    expect(sameBranch("fix-login", "fix-logout")).toBe(false);
  });
});
