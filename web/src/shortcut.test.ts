import { describe, expect, it } from "vitest";
import { isShortcut } from "./shortcut";

/** A keydown on `target` (the page, or inside the terminal). */
function press(key: string, mods: { metaKey?: boolean; ctrlKey?: boolean; altKey?: boolean; shiftKey?: boolean } = {}, target: Element = document.body) {
  const event = new KeyboardEvent("keydown", { key, ...mods });
  Object.defineProperty(event, "target", { value: target });
  return event;
}

const terminal = () => {
  const xterm = document.createElement("div");
  xterm.className = "xterm";
  const inside = document.createElement("textarea");
  xterm.append(inside);
  return inside;
};

describe("isShortcut", () => {
  it("takes Cmd with the key anywhere, the terminal too, in either case", () => {
    expect(isShortcut(press("k", { metaKey: true }), "k")).toBe(true);
    expect(isShortcut(press("K", { metaKey: true }), "k")).toBe(true);
    expect(isShortcut(press("k", { metaKey: true }, terminal()), "k")).toBe(true);
    expect(isShortcut(press("\\", { metaKey: true }), "\\")).toBe(true);
  });

  it("takes Ctrl with the key outside the terminal, and leaves Ctrl to the shell inside it", () => {
    expect(isShortcut(press("f", { ctrlKey: true }), "f")).toBe(true);
    expect(isShortcut(press("f", { ctrlKey: true }, terminal()), "f")).toBe(false);
  });

  it("ignores the key alone, another key, and the key with Alt or Shift", () => {
    expect(isShortcut(press("k"), "k")).toBe(false);
    expect(isShortcut(press("j", { metaKey: true }), "k")).toBe(false);
    expect(isShortcut(press("k", { metaKey: true, altKey: true }), "k")).toBe(false);
    expect(isShortcut(press("k", { metaKey: true, shiftKey: true }), "k")).toBe(false);
  });
});
