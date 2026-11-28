import { fireEvent, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { isShortcut, typing, useSettingsKey } from "./shortcut";

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

describe("typing", () => {
  it("is true in a field, a select, an editable element and the terminal, and not elsewhere", () => {
    const field = document.createElement("input");
    expect(typing(field)).toBe(true);
    expect(typing(document.createElement("select"))).toBe(true);
    expect(typing(terminal())).toBe(true);
    const editable = document.createElement("div");
    Object.defineProperty(editable, "isContentEditable", { value: true });
    expect(typing(editable)).toBe(true);
    expect(typing(document.createElement("button"))).toBe(false);
    expect(typing(document.body)).toBe(false);
    expect(typing(null)).toBe(false);
  });
});

describe("useSettingsKey", () => {
  /** The hook mounted on `run`, on a page whose platform is `platform`. */
  function mounted(run: () => void, platform = "Linux x86_64") {
    vi.spyOn(navigator, "platform", "get").mockReturnValue(platform);
    const task = document.createElement("input");
    const inTerminal = terminal();
    document.body.append(task, inTerminal.parentElement!);
    const view = renderHook(() => useSettingsKey(run));
    return {
      task,
      inTerminal,
      done: () => {
        view.unmount();
        task.remove();
        inTerminal.parentElement!.remove();
        vi.restoreAllMocks();
      },
    };
  }

  it("runs on Cmd+, on macOS, and not on Ctrl+, there", () => {
    const run = vi.fn();
    const { done } = mounted(run, "MacIntel");
    fireEvent.keyDown(document.body, { key: ",", ctrlKey: true });
    expect(run).not.toHaveBeenCalled();
    fireEvent.keyDown(document.body, { key: ",", metaKey: true });
    expect(run).toHaveBeenCalledTimes(1);
    done();
  });

  it("runs on Ctrl+, elsewhere, and not on Cmd+, there", () => {
    const run = vi.fn();
    const { done } = mounted(run);
    fireEvent.keyDown(document.body, { key: ",", metaKey: true });
    expect(run).not.toHaveBeenCalled();
    fireEvent.keyDown(document.body, { key: ",", ctrlKey: true });
    expect(run).toHaveBeenCalledTimes(1);
    done();
  });

  it("does not run with the other modifier held too", () => {
    const run = vi.fn();
    const { done } = mounted(run, "MacIntel");
    fireEvent.keyDown(document.body, { key: ",", metaKey: true, ctrlKey: true });
    expect(run).not.toHaveBeenCalled();
    done();
  });

  it("does not run on the key alone, or with Alt or Shift", () => {
    const run = vi.fn();
    const { done } = mounted(run);
    fireEvent.keyDown(document.body, { key: "," });
    fireEvent.keyDown(document.body, { key: ",", ctrlKey: true, altKey: true });
    fireEvent.keyDown(document.body, { key: ",", ctrlKey: true, shiftKey: true });
    expect(run).not.toHaveBeenCalled();
    done();
  });

  it("does not run while a text field or a terminal has focus", () => {
    const run = vi.fn();
    const { task, inTerminal, done } = mounted(run);
    fireEvent.keyDown(task, { key: ",", ctrlKey: true });
    fireEvent.keyDown(inTerminal, { key: ",", ctrlKey: true });
    expect(run).not.toHaveBeenCalled();
    done();
  });
});
