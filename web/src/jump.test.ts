import { describe, expect, it } from "vitest";
import { fleet } from "./fleet";
import { destinations, newWorkspace, search, type Command } from "./jump";
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
    expect(all[0]).toMatchObject({ kind: "terminal", terminal: "t1", label: "Needs you", glyph: "waiting", machine: "studio" });
    expect(all[1]).toMatchObject({ kind: "terminal", terminal: "t2", label: "Failed · exit 2", glyph: "failed" });
    expect(all[2]).toMatchObject({ machine: "mini", glyph: "working" });
    expect(all[3]).toMatchObject({ kind: "shell", workspace: "w1" });
  });
});

describe("search", () => {
  it("keeps every destination for an empty query", () => {
    expect(search(destinations(machines()), "  ")).toHaveLength(5);
  });

  it("keeps a destination only when every word is in its workspace, name, state, project, branch or machine", () => {
    const titles = (query: string) => search(destinations(machines()), query).map((d) => d.title);
    expect(titles("LOGIN needs")).toEqual(["fix login / sonnet"]);
    expect(titles("mini")).toEqual(["tidy readme / opus", "New shell in tidy readme"]);
    expect(titles("demo failed")).toEqual(["fix login / shell"]);
    expect(titles("new shell tidy")).toEqual(["New shell in tidy readme"]);
    expect(titles("nothing-like-this")).toEqual([]);
  });
});

describe("briefs in the palette", () => {
  it("finds a session by its title and shows the title under its name", () => {
    const withBriefs = fleet(
      [workspace(ME, "w1", "fix login"), terminal(ME, "t1", "w1"), session(ME, "s1", "w1", "t1", "idle"), terminal(ME, "t2", "w1"), session(ME, "s2", "w1", "t2", "idle")],
      settings(),
      { s1: { title: "make the button blue", now: null }, s2: { title: "write the release notes", now: null } },
    );
    const found = search(destinations(withBriefs), "release");
    expect(found).toHaveLength(1);
    expect(found[0]).toMatchObject({ kind: "terminal", terminal: "t2", title: "fix login / opus 2", subtitle: "write the release notes" });
  });
});

describe("newWorkspace", () => {
  it("names a new workspace with what was typed, and offers none for nothing", () => {
    expect(newWorkspace("  add   search ")).toEqual([expect.objectContaining({ kind: "workspace", name: "add search", title: "New task “add search”" })]);
    expect(newWorkspace("   ")).toEqual([]);
  });
});

describe("views and actions in the palette", () => {
  const command = (key: string, title: string, also = ""): Command => ({ key, title, group: "Go to", about: `About ${title}`, also, run: () => {} });
  const commands = [command("timeline", "Timeline", "turns clock"), command("factory", "Factory", "issues runs")];

  it("lists them after the fleet's terminals and before the new shells", () => {
    const all = destinations(machines(), commands);
    expect(all.map((d) => d.title)).toEqual(["fix login / sonnet", "fix login / shell", "tidy readme / opus", "Timeline", "Factory", "New shell in fix login", "New shell in tidy readme"]);
    expect(all[3]).toMatchObject({ kind: "command", key: "command:timeline", command: commands[0] });
  });

  it("finds one by its name or the other words it carries, and puts it first once something is typed", () => {
    const withFactory = fleet([workspace(ME, "w1", "fix the factory floor"), terminal(ME, "t1", "w1"), session(ME, "s1", "w1", "t1", "idle")], settings());
    const all = destinations(withFactory, commands);
    expect(search(all, "turns").map((d) => d.title)).toEqual(["Timeline"]);
    expect(search(all, "factory").map((d) => d.title)).toEqual(["Factory", "fix the factory floor / opus", "New shell in fix the factory floor"]);
    expect(search(all, "").map((d) => d.title)).toEqual(["fix the factory floor / opus", "Timeline", "Factory", "New shell in fix the factory floor"]);
  });
});
