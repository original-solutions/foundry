import { describe, expect, it } from "vitest";
import type { Mark } from "./api";
import { pathOf, touched } from "./touched";

const tool = (name: string, text: string, at = 0): Mark => ({ kind: "tool", tool: name, text, at });

describe("pathOf", () => {
  it("is the file an edit, write or read acts on, as the trail names it", () => {
    expect(pathOf(tool("Edit", "Edit src/a.rs"))).toBe("src/a.rs");
    expect(pathOf(tool("Write", "Write docs/new page.md"))).toBe("docs/new page.md");
    expect(pathOf(tool("MultiEdit", "MultiEdit src/b.rs"))).toBe("src/b.rs");
    expect(pathOf(tool("NotebookEdit", "NotebookEdit nb/x.ipynb"))).toBe("nb/x.ipynb");
    expect(pathOf(tool("Read", "Read README.md"))).toBe("README.md");
  });

  it("is nothing for a command, a search, a question, a tool with no path, or a path cut short", () => {
    expect(pathOf(tool("Bash", "$ cat src/a.rs"))).toBeUndefined();
    expect(pathOf(tool("Grep", "Grep fn main"))).toBeUndefined();
    expect(pathOf({ kind: "ask", text: "Edit src/a.rs?", at: 0 })).toBeUndefined();
    expect(pathOf(tool("Edit", "Edit"))).toBeUndefined();
    expect(pathOf(tool("Read", "Read src/very/long/pa…"))).toBeUndefined();
  });
});

describe("touched", () => {
  const trail: Mark[] = [
    { kind: "prompt", text: "retry webhooks", at: 0 },
    tool("Read", "Read src/webhooks.rs"),
    tool("Read", "Read src/lib.rs"),
    tool("Bash", "$ cargo test"),
    tool("Edit", "Edit src/webhooks.rs"),
    tool("Bash", "$ cargo test"),
    { kind: "done", text: "", at: 0 },
    { kind: "prompt", text: "and the readme", at: 0 },
    tool("Edit", "Edit README.md"),
    tool("Grep", "Grep retry"),
    { kind: "ask", text: "Claude needs your permission to use Bash", at: 0 },
    tool("Bash", "$ cargo fmt"),
    tool("Edit", "Edit src/webhooks.rs"),
  ];

  it("lists each file once with its edits and reads, edited files first, the most recently touched first", () => {
    const { files } = touched(trail);
    expect(files.map((f) => [f.path, f.edits, f.reads])).toEqual([
      ["src/webhooks.rs", 2, 1],
      ["README.md", 1, 0],
      ["src/lib.rs", 0, 1],
    ]);
  });

  it("places every touch along the trail, from 0 at its first mark to 1 at its last", () => {
    const { files } = touched(trail);
    expect(files[0].touches).toEqual([
      { at: 1 / 12, edit: false },
      { at: 4 / 12, edit: true },
      { at: 1, edit: true },
    ]);
  });

  it("counts each command once with how often it ran, the most recent first", () => {
    const { commands } = touched(trail);
    expect(commands.map((c) => [c.text, c.count])).toEqual([
      ["cargo fmt", 1],
      ["cargo test", 2],
    ]);
    expect(commands[1].touches.map((t) => t.at)).toEqual([3 / 12, 5 / 12]);
  });

  it("keeps searches and other tools apart from files and commands, and leaves out prompts and questions", () => {
    const { others } = touched(trail);
    expect(others.map((o) => [o.text, o.count, o.kind])).toEqual([["Grep retry", 1, "search"]]);
  });

  it("places the start of every turn after the first along the same trail, to mark where turns meet", () => {
    expect(touched(trail).turns).toEqual([7 / 12]);
  });

  it("is empty for an empty trail, and places a one-mark trail at its start", () => {
    expect(touched([])).toEqual({ files: [], commands: [], others: [], turns: [] });
    expect(touched([tool("Read", "Read a.md")]).files[0].touches).toEqual([{ at: 0, edit: false }]);
  });
});
