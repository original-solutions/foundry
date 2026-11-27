import { describe, expect, it } from "vitest";
import { languageOf, lex, type Token } from "./syntax";

/** A line's tokens as `tone:text` for the coloured ones, plain text left as it is. */
function shown(line: Token[]) {
  return line.map((t) => (t.tone ? `${t.tone}:${t.text}` : t.text));
}

function lexed(path: string, ...lines: string[]) {
  const lang = languageOf(path);
  if (!lang) throw new Error(`no language for ${path}`);
  return lex(lines, lang).map(shown);
}

describe("languageOf", () => {
  it("knows a language by the file's extension, and none for a file it cannot colour", () => {
    for (const path of ["src/a.ts", "App.tsx", "x.js", "x.mjs", "src/main.rs", "cmd/main.go", "run.py", "build.sh", "a.css", "package.json", "ci.yml", "Cargo.toml", "README.md"]) {
      expect(languageOf(path), path).toBeDefined();
    }
    expect(languageOf("logo.png")).toBeUndefined();
    expect(languageOf("LICENSE")).toBeUndefined();
  });
});

describe("lex", () => {
  it("colours keywords, strings, numbers, literals and constants, types and calls, and leaves the rest plain", () => {
    expect(lexed("a.ts", 'export const LIMIT = 5; if (!ok) return failed(email, "Too many", true) as Result;')).toEqual([
      [
        "keyword:export",
        " ",
        "keyword:const",
        " ",
        "number:LIMIT",
        " = ",
        "number:5",
        "; ",
        "keyword:if",
        " (!ok) ",
        "keyword:return",
        " ",
        "call:failed",
        "(email, ",
        'string:"Too many"',
        ", ",
        "number:true",
        ") ",
        "keyword:as",
        " ",
        "type:Result",
        ";",
      ],
    ]);
  });

  it("keeps the text whole: the tokens of a line join back to the line", () => {
    const line = "  const failures = new Map<string, number[]>(); // per email";
    expect(lex([line], languageOf("a.ts")!)[0].map((t) => t.text).join("")).toBe(line);
  });

  it("colours a comment to the end of the line, and a comment marker inside a string is no comment", () => {
    expect(lexed("a.ts", 'const url = "http://x"; // the host')).toEqual([["keyword:const", " url = ", 'string:"http://x"', "; ", "comment:// the host"]]);
    expect(lexed("run.py", "x = 1  # one")).toEqual([["x = ", "number:1", "  ", "comment:# one"]]);
  });

  it("carries a block comment and a long string from one line to the next", () => {
    expect(lexed("a.ts", "/** Failed logins", "    in a window. */ const a = `one", "two` + b;")).toEqual([
      ["comment:/** Failed logins"],
      ["comment:    in a window. */", " ", "keyword:const", " a = ", "string:`one"],
      ["string:two`", " + b;"],
    ]);
    expect(lexed("run.py", 'doc = """Start', 'end""" if x else None')).toEqual([
      ["doc = ", 'string:"""Start'],
      ['string:end"""', " ", "keyword:if", " x ", "keyword:else", " ", "number:None"],
    ]);
  });

  it("does not run a short string past its line, and skips an escaped quote", () => {
    expect(lexed("a.ts", 'a = "open', 'b = "say \\"hi\\"" + c')).toEqual([["a = ", 'string:"open'], ["b = ", 'string:"say \\"hi\\""', " + c"]]);
  });

  it("reads an apostrophe in a word as no quote", () => {
    expect(lexed("build.sh", "echo don't stop # now")).toEqual([["echo don't stop ", "comment:# now"]]);
  });

  it("reads numbers with their units and suffixes, and not digits inside a name", () => {
    expect(lexed("a.css", "a { margin: 10px 0.5em; } /* x2 */")).toEqual([["a { margin: ", "number:10px", " ", "number:0.5em", "; } ", "comment:/* x2 */"]]);
    expect(lexed("a.rs", "let x2 = 0x1F_u32;")).toEqual([["keyword:let", " x2 = ", "number:0x1F_u32", ";"]]);
  });

  it("colours a Markdown heading, inline code and a fenced block", () => {
    expect(lexed("README.md", "## Login", "Run `npm test` now.", "```sh", "npm run dev", "```", "done")).toEqual([
      ["heading:## Login"],
      ["Run ", "string:`npm test`", " now."],
      ["string:```sh"],
      ["string:npm run dev"],
      ["string:```"],
      ["done"],
    ]);
  });

  it("colours a key in YAML and TOML, and a TOML table as a heading", () => {
    expect(lexed("ci.yml", "jobs:", "  - name: test # it", "    on: true")).toEqual([
      ["keyword:jobs", ":"],
      ["  - ", "keyword:name", ": test ", "comment:# it"],
      ["    ", "keyword:on", ": ", "number:true"],
    ]);
    expect(lexed("Cargo.toml", "[package]", 'name = "foundry"')).toEqual([["heading:[package]"], ["keyword:name", " = ", 'string:"foundry"']]);
  });
});
