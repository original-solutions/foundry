import { describe, expect, it } from "vitest";
import { fleet } from "./fleet";
import { destinations, search } from "./jump";
import { ME, OTHER, sample, session, settings, terminal, workspace } from "./test-fixtures";

const machines = () =>
  fleet(
    [
      sample(ME, 1),
      sample(OTHER, 1),
      workspace(ME, "w1", "fix login"),
      terminal(ME, "t1", "w1", "running"),
      session(ME, "s1", "w1", "t1", "waiting_for_you", { model: "sonnet" }),
      terminal(ME, "t2", "w1", "exited", { exit_code: 2 }),
      workspace(OTHER, "w2", "tidy readme"),
      terminal(OTHER, "t3", "w2", "running"),
      session(OTHER, "s3", "w2", "t3", "working"),
    ],
    settings({ machines: [{ id: ME, name: "studio", address: "a" }, { id: OTHER, name: "mini", address: "b" }] }),
  );

describe("destinations", () => {
  it("lists every terminal in the fleet with its workspace, name, state and machine, then a new shell per workspace", () => {
    const all = destinations(machines());
    expect(all.map((d) => d.title)).toEqual(["fix login / sonnet", "fix login / shell", "tidy readme / opus", "New shell in fix login", "New shell in tidy readme"]);
    expect(all[0]).toMatchObject({ kind: "terminal", terminal: "t1", label: "Waiting for you", tone: "attention", machine: "studio" });
    expect(all[1]).toMatchObject({ kind: "terminal", terminal: "t2", label: "Exited 2", tone: "bad" });
    expect(all[2]).toMatchObject({ machine: "mini", tone: "busy" });
    expect(all[3]).toMatchObject({ kind: "shell", workspace: "w1" });
  });
});

describe("search", () => {
  it("keeps every destination for an empty query", () => {
    expect(search(destinations(machines()), "  ")).toHaveLength(5);
  });

  it("keeps a destination only when every word is in its workspace, name, state, project, branch or machine", () => {
    const titles = (query: string) => search(destinations(machines()), query).map((d) => d.title);
    expect(titles("LOGIN waiting")).toEqual(["fix login / sonnet"]);
    expect(titles("mini")).toEqual(["tidy readme / opus", "New shell in tidy readme"]);
    expect(titles("demo exited")).toEqual(["fix login / shell"]);
    expect(titles("new shell tidy")).toEqual(["New shell in tidy readme"]);
    expect(titles("nothing-like-this")).toEqual([]);
  });
});
