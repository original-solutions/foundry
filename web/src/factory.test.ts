import { describe, expect, it } from "vitest";
import type { FactoryStatus } from "./api";
import { githubRepo, issueUrl, parseRun, settingsLine, tally } from "./factory";

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

describe("tally", () => {
  const empty: FactoryStatus = {
    project: "p",
    pass: 1,
    holds: true,
    generation: 1,
    not_holding: null,
    idle: null,
    owner_ignored: false,
    ready: [],
    feedback_waiting: [],
    runs: [],
    held_up: [],
    gate_infra: [],
    finished: [],
    foreign: [],
  };
  const run = { run: "a__b__1__1", title: "t", step: "s", machine: "m", gate_retries_used: 0, minutes_used: 0, workspaces: [], sessions: [] };

  it("counts each station across the statuses, and a missing status as nothing", () => {
    const one = { ...empty, ready: [{ issue: 1, title: "a", reason: "r" }], runs: [run], finished: [{ run: "a__b__2__1", title: "x", outcome: "pr-open" as const, pr_end: null, pr_url: null }] };
    const two = { ...empty, ready: [{ issue: 3, title: "b", reason: "r" }], held_up: [{ run: "a__b__4__1", kind: "stuck" as const, reason: "r" }] };
    expect(tally([one, undefined, two])).toEqual({ ready: 2, building: 1, held: 1, finished: 1 });
  });

  it("counts nothing for no statuses", () => {
    expect(tally([])).toEqual({ ready: 0, building: 0, held: 0, finished: 0 });
  });
});

describe("settingsLine", () => {
  it("says the fleet's settings in one line, each number with its unit", () => {
    expect(settingsLine({ paused: false, model: "opus", effort: "high", max_runs: 1, max_gates: 3, poll_seconds: 30, feedback_seconds: 300 })).toBe(
      "opus · high effort · 1 run at once · 3 gates at once · looks every 30s · feedback every 300s",
    );
  });
});
