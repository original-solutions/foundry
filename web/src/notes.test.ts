import { describe, expect, it } from "vitest";
import { diffLines } from "./changes";
import type { SessionView } from "./fleet";
import { anchor, onLine, review, takers, type Note } from "./notes";

const lines = diffLines("@@ -4,2 +4,3 @@\n const a = 1;\n-const b = 2;\n+const b = 3;\n+const c = 4;\n");

const note = (over: Partial<Note>): Note => ({ id: 1, path: "src/login.ts", side: "new", line: 5, code: "const b = 3;", body: "call it total", ...over });

describe("anchor", () => {
  it("pins added and unchanged lines to the new file's numbers and removed lines to the old file's", () => {
    expect(lines.map(anchor)).toEqual([
      undefined,
      { side: "new", line: 4, code: "const a = 1;" },
      { side: "old", line: 5, code: "const b = 2;" },
      { side: "new", line: 5, code: "const b = 3;" },
      { side: "new", line: 6, code: "const c = 4;" },
    ]);
  });

  it("finds a note's line only while that line still reads as it did", () => {
    const at = anchor(lines[3])!;
    expect(onLine(note({}), "src/login.ts", at)).toBe(true);
    expect(onLine(note({ code: "const b = 9;" }), "src/login.ts", at)).toBe(false);
    expect(onLine(note({}), "src/other.ts", at)).toBe(false);
    expect(onLine(note({ side: "old" }), "src/login.ts", at)).toBe(false);
  });
});

describe("review", () => {
  it("numbers each note with where it is, the line it is on, and what to do", () => {
    const text = review([
      note({}),
      note({ id: 2, path: "README.md", side: "old", line: 3, code: "Old intro", body: "keep this line\nbut shorter" }),
    ]);
    expect(text).toBe(
      [
        "Review comments on your changes. Address each one, then say what you changed.",
        "",
        "1. src/login.ts:5",
        "   > const b = 3;",
        "   call it total",
        "",
        "2. README.md, removed line 3",
        "   > Old intro",
        "   keep this line",
        "   but shorter",
      ].join("\n"),
    );
  });

  it("drops control characters, so nothing in a note can end the paste early or press Enter", () => {
    const text = review([note({ code: "a\x1b[201~b\r", body: "x\x1b[201~\ry\tz" })]);
    expect([...text].filter((c) => c !== "\n" && c !== "\t" && (c < " " || c === "\x7f"))).toEqual([]);
    expect(text).toContain("> a[201~b");
    expect(text).toContain("x[201~y\tz");
  });

  it("cuts a very long line of code, keeping the note whole", () => {
    const text = review([note({ code: "x".repeat(500), body: "y".repeat(500) })]);
    expect(text).toContain(`> ${"x".repeat(119)}…`);
    expect(text).toContain("y".repeat(500));
  });
});

describe("takers", () => {
  const session = (over: Partial<SessionView>): SessionView => ({ id: "s1", model: "opus", name: "opus", state: "idle", terminal: "t1", ...over });

  it("names each session as the sidebar does, with its first prompt after it when it has one", () => {
    const list = takers([
      session({ name: "opus 1" }),
      session({ id: "s2", terminal: "t2", name: "opus 2", state: "working" }),
      session({ id: "s3", terminal: "t3", model: "sonnet", name: "sonnet", title: "Fix the login check" }),
    ]);
    expect(list.map((t) => [t.name, t.ready])).toEqual([
      ["opus 1", true],
      ["opus 2", false],
      ["sonnet · Fix the login check", true],
    ]);
  });

  it("says a live factory run's stuck session is quiet, and a held-up run's stuck", () => {
    expect(takers([session({ state: "stuck" })], { run: "o__r__162__1" })[0].label).toBe("Quiet 10+ min");
    expect(takers([session({ state: "stuck" })], { run: "o__r__162__1", held: "stuck" })[0].label).toBe("Stuck");
    expect(takers([session({ state: "stuck" })])[0].label).toBe("Stuck");
  });

  it("keeps a lone model's name plain and says what a busy session is doing", () => {
    const [only] = takers([session({ state: "working" })]);
    expect(only.name).toBe("opus");
    expect(only.ready).toBe(false);
    expect(only.label).toBe("Working");
    expect(only.glyph).toBe("working");
  });
});
