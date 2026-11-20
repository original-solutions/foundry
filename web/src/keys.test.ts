import { describe, expect, it } from "vitest";
import { keyData, keysFor } from "./keys";

describe("keysFor", () => {
  it("offers the keys a phone keyboard lacks: escape, shift+tab, tab, arrows, ctrl+c and enter", () => {
    expect(keysFor("ready").map((k) => k.label)).toEqual(["esc", "⇧tab", "tab", "↑", "↓", "←", "→", "^C", "⏎"]);
  });

  it("puts the answer keys 1 to 3 first while a session waits for you", () => {
    const keys = keysFor("waiting");
    expect(keys.slice(0, 3).map((k) => [k.label, k.answer])).toEqual([
      ["1", true],
      ["2", true],
      ["3", true],
    ]);
    expect(keys.slice(3).map((k) => k.label)).toEqual(keysFor("ready").map((k) => k.label));
  });

  it("offers no answer keys in any other state", () => {
    for (const glyph of ["starting", "working", "ready", "done", "failed", "stuck", "away"] as const) {
      expect(keysFor(glyph).some((k) => k.answer)).toBe(false);
    }
  });
});

describe("keyData", () => {
  const key = (label: string) => keysFor("waiting").find((k) => k.label === label)!;

  it("sends the bytes a real keyboard sends", () => {
    const normal = { applicationCursorKeysMode: false };
    expect(keyData(key("esc"), normal)).toBe("\x1b");
    expect(keyData(key("⇧tab"), normal)).toBe("\x1b[Z");
    expect(keyData(key("tab"), normal)).toBe("\t");
    expect(keyData(key("^C"), normal)).toBe("\x03");
    expect(keyData(key("⏎"), normal)).toBe("\r");
    expect(keyData(key("2"), normal)).toBe("2");
  });

  it("sends arrows in the form the program asked for", () => {
    expect(["↑", "↓", "→", "←"].map((l) => keyData(key(l), { applicationCursorKeysMode: false }))).toEqual(["\x1b[A", "\x1b[B", "\x1b[C", "\x1b[D"]);
    expect(["↑", "↓", "→", "←"].map((l) => keyData(key(l), { applicationCursorKeysMode: true }))).toEqual(["\x1bOA", "\x1bOB", "\x1bOC", "\x1bOD"]);
  });
});
