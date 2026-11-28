import { describe, expect, it } from "vitest";
import { byTodo, firstLine, heading, type AttentionItem } from "./attention";

const item = (id: string, kind: string, over: Partial<AttentionItem> = {}): AttentionItem => ({
  id,
  kind,
  machine: "m1",
  machine_name: "trilogy-mac",
  what: `what of ${id}`,
  todo: `todo of ${kind}`,
  terminal: null,
  issue_url: null,
  pr_url: null,
  ...over,
});

describe("byTodo", () => {
  it("puts items of one kind in one group, in the order the kinds first come", () => {
    const a = item("run_stuck:r1", "run_stuck");
    const b = item("machine_offline:m3", "machine_offline");
    const c = item("run_stuck:r2", "run_stuck");
    expect(byTodo([a, b, c]).map((group) => [group.kind, group.items.map((i) => i.id)])).toEqual([
      ["run_stuck", ["run_stuck:r1", "run_stuck:r2"]],
      ["machine_offline", ["machine_offline:m3"]],
    ]);
  });

  it("gives a group the todo its items share", () => {
    const [group] = byTodo([item("run_stuck:r1", "run_stuck"), item("run_stuck:r2", "run_stuck")]);
    expect(group.todo).toBe("todo of run_stuck");
  });

  it("puts items of one kind that say different things in a group per todo, in the order each first comes", () => {
    const groups = byTodo([
      item("run_stuck:r1", "run_stuck", { todo: "check the branch" }),
      item("machine_offline:m3", "machine_offline"),
      item("run_stuck:r2", "run_stuck", { todo: "read the comment" }),
      item("run_stuck:r3", "run_stuck", { todo: "check the branch" }),
    ]);
    expect(groups.map((group) => [group.kind, group.todo, group.items.map((i) => i.id)])).toEqual([
      ["run_stuck", "check the branch", ["run_stuck:r1", "run_stuck:r3"]],
      ["machine_offline", "todo of machine_offline", ["machine_offline:m3"]],
      ["run_stuck", "read the comment", ["run_stuck:r2"]],
    ]);
  });

  it("keeps a todo of one kind apart from the same todo of another", () => {
    const groups = byTodo([item("run_stuck:r1", "run_stuck", { todo: "same" }), item("gate_infra:r2", "gate_infra", { todo: "same" })]);
    expect(groups.map((group) => group.kind)).toEqual(["run_stuck", "gate_infra"]);
  });

  it("keeps apart two groups whose kind and todo run together into the same text", () => {
    const groups = byTodo([item("a:1", "a", { todo: "bc" }), item("ab:1", "ab", { todo: "c" })]);
    expect(groups.map((group) => [group.kind, group.todo])).toEqual([
      ["a", "bc"],
      ["ab", "c"],
    ]);
    expect(new Set(groups.map((group) => group.key)).size).toBe(2);
  });

  it("gives each group a key of its kind and todo, the same for the same pair", () => {
    const [one, two] = byTodo([item("run_stuck:r1", "run_stuck", { todo: "same" }), item("gate_infra:r2", "gate_infra", { todo: "same" })]);
    expect(one.key).not.toBe(two.key);
    const [again] = byTodo([item("run_stuck:r9", "run_stuck", { todo: "same" })]);
    expect(again.key).toBe(one.key);
  });

  it("gives a group the machine its items share, and none when they are on different machines", () => {
    const [same] = byTodo([item("run_stuck:r1", "run_stuck"), item("run_stuck:r2", "run_stuck")]);
    expect(same.machine_name).toBe("trilogy-mac");
    const [mixed] = byTodo([item("disk_low:m1", "disk_low"), item("disk_low:m2", "disk_low", { machine: "m2", machine_name: "build-box" })]);
    expect(mixed.machine_name).toBeNull();
  });

  it("gives nothing for no items", () => {
    expect(byTodo([])).toEqual([]);
  });
});

describe("heading", () => {
  it("names one item by its kind's label", () => {
    expect(heading("run_stuck", 1)).toBe("Run stuck");
  });

  it("counts several items of a kind", () => {
    expect(heading("run_stuck", 16)).toBe("16 runs stuck");
    expect(heading("machine_offline", 2)).toBe("2 machines offline");
  });

  it.each(["run_stuck", "run_needs_info", "factory_idle", "foreign_claim", "gate_infra"])("has a label for the daemon's %s", (kind) => {
    expect(heading(kind, 1)).not.toBe(kind);
    expect(heading(kind, 2)).not.toContain(kind);
  });

  it("falls back to the raw kind for a kind this build does not know", () => {
    expect(heading("lease_lost", 1)).toBe("lease_lost");
    expect(heading("lease_lost", 3)).toBe("3 lease_lost");
  });
});

describe("firstLine", () => {
  it("is the text up to its first line break", () => {
    expect(firstLine("make check failed\n\nReportingEdgeTest > a\nReportingEdgeTest > b")).toBe("make check failed");
  });

  it("is the whole text when it has one line", () => {
    expect(firstLine("build-box has not been heard from")).toBe("build-box has not been heard from");
  });
});
