import { describe, expect, it } from "vitest";
import { linksIn, rowText, type Row } from "./links";

const files = ["src/version.ts", "RELEASES.md", "web/src/App.tsx", "crates/a/src/App.tsx"];
const found = (text: string, list = files) => linksIn(text, list).map((l) => ({ ...l, shown: text.slice(l.start, l.end) }));

describe("linksIn", () => {
  it("finds a web address and leaves the sentence's closing punctuation out of it", () => {
    const text = "Opened https://github.com/o/r/pull/12. See (https://example.com/a_(b)) too";
    expect(found(text)).toEqual([
      { start: 7, end: 37, kind: "url", target: "https://github.com/o/r/pull/12", shown: "https://github.com/o/r/pull/12" },
      { start: 44, end: 69, kind: "url", target: "https://example.com/a_(b)", shown: "https://example.com/a_(b)" },
    ]);
  });

  it("finds a changed file named by its path, from ./, inside an absolute path or with a line number", () => {
    const text = "Edit(src/version.ts) then ./RELEASES.md:12 and /Users/x/wt/web/src/App.tsx.";
    expect(found(text).map((l) => [l.shown, l.target])).toEqual([
      ["src/version.ts", "src/version.ts"],
      ["./RELEASES.md", "RELEASES.md"],
      ["/Users/x/wt/web/src/App.tsx", "web/src/App.tsx"],
    ]);
  });

  it("finds a file named by its name alone only when one changed file has that name", () => {
    expect(found("Updated version.ts and App.tsx").map((l) => [l.shown, l.target])).toEqual([["version.ts", "src/version.ts"]]);
  });

  it("finds nothing in a word that only ends like a changed file, or in a file the workspace did not change", () => {
    expect(found("myversion.ts other/src/versions.ts README.md")).toEqual([]);
    expect(found("src/version.ts", [])).toEqual([]);
  });

  it("does not take a path inside a web address as a file", () => {
    expect(found("https://github.com/o/r/blob/main/src/version.ts").map((l) => l.kind)).toEqual(["url"]);
  });
});

/** A buffer row as xterm keeps it: cells with their characters and widths, and whether it carries on the row above. */
function row(text: string, wrapped = false, wide: number[] = []): Row {
  const cells: { chars: string; width: number }[] = [];
  for (const [i, ch] of [...text].entries()) {
    if (wide.includes(i)) cells.push({ chars: ch, width: 2 }, { chars: "", width: 0 });
    else cells.push({ chars: ch, width: 1 });
  }
  return { isWrapped: wrapped, length: cells.length, getCell: (x: number) => cells[x] && { getChars: () => cells[x].chars, getWidth: () => cells[x].width } };
}

describe("rowText", () => {
  it("joins a line wrapped over rows and says which cell each character sits in", () => {
    const rows = [row("see https://exa"), row("mple.com/x now", true), row("next line")];
    const { text, cells } = rowText((y) => rows[y], 1);
    expect(text).toBe("see https://example.com/x now");
    expect(cells[4]).toEqual({ x: 4, y: 0 });
    expect(cells[15]).toEqual({ x: 0, y: 1 });
  });

  it("counts a wide character as the two cells it takes", () => {
    const { text, cells } = rowText(() => row("🧠 a.ts", false, [0]), 0);
    expect(text).toBe("🧠 a.ts");
    // The emoji is two UTF-16 units in one wide cell; the space after it is in cell 2.
    expect(cells[2]).toEqual({ x: 2, y: 0 });
    expect(cells[3]).toEqual({ x: 3, y: 0 });
  });
});
