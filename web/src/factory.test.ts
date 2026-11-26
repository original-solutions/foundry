import { describe, expect, it } from "vitest";
import { githubRepo, issueUrl, parseRun } from "./factory";

describe("parseRun", () => {
  it.each([
    ["tom_tc__softfac__8__1", { owner: "tom_tc", repo: "softfac", issue: 8, attempt: 1 }],
    ["tom_tc__a__b__8__1", { owner: "tom_tc", repo: "a__b", issue: 8, attempt: 1 }],
    ["a_b_c___x___8__1", { owner: "a_b_c", repo: "_x_", issue: 8, attempt: 1 }],
    ["rawphp__my_app__12__3", { owner: "rawphp", repo: "my_app", issue: 12, attempt: 3 }],
    ["rawphp__a__b__8__1", { owner: "rawphp", repo: "a__b", issue: 8, attempt: 1 }],
    ["x__y__0__0", { owner: "x", repo: "y", issue: 0, attempt: 0 }],
  ])("takes %s apart", (id, parts) => {
    expect(parseRun(id)).toEqual(parts);
  });

  it.each(["", "nope", "a__b", "a__b__8", "a__b__x__1", "a__b__8__y", "a__b__08__1", "a__b__8__-1", "__b__8__1", "a____8__1"])("refuses %j", (id) => {
    expect(parseRun(id)).toBeUndefined();
  });

  it("links an issue of its repo", () => {
    expect(issueUrl({ owner: "acme", repo: "foundry", issue: 12 })).toBe("https://github.com/acme/foundry/issues/12");
  });
});

describe("githubRepo", () => {
  it("reads an https GitHub repo, ignoring a user or hidden password", () => {
    expect(githubRepo("https://github.com/acme/foundry.git")).toEqual({ owner: "acme", repo: "foundry" });
    expect(githubRepo("https://***@github.com/acme/foundry")).toEqual({ owner: "acme", repo: "foundry" });
  });
  it("knows nothing of an SSH URL or another host", () => {
    expect(githubRepo("git@github.com:acme/foundry.git")).toBeUndefined();
    expect(githubRepo("/tmp/demo.git")).toBeUndefined();
  });
});
