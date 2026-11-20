import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { CURRENT, FindBar, type Findable } from "./FindBar";

/**
 * A stand-in for xterm: `lines` is its whole buffer (the cursor on the last line, the screen
 * showing the bottom ten). It records what the bar does to it, and `lit` lists the hits painted
 * now, the one you are on first.
 */
function fakeTerm(lines: string[]) {
  const did: string[] = [];
  const paints = new Set<{ text: string; current: boolean }>();
  let parsed: (() => void) | undefined;
  let scrolled: (() => void) | undefined;
  const active = {
    get length() {
      return lines.length;
    },
    viewportY: Math.max(0, lines.length - 10),
    get baseY() {
      return Math.max(0, lines.length - 10);
    },
    get cursorY() {
      return Math.min(lines.length, 10) - 1;
    },
    getLine: (i: number) => (i < lines.length ? { isWrapped: false, translateToString: () => lines[i] } : undefined),
  };
  const term = {
    rows: 10,
    buffer: { active },
    registerMarker: (offset = 0) => ({ line: active.baseY + active.cursorY + offset, dispose() {} }),
    registerDecoration: ({ marker, x, width, backgroundColor }: { marker: { line: number }; x?: number; width?: number; backgroundColor?: string }) => {
      const paint = { text: `${marker.line}:${x}+${width}`, current: backgroundColor === CURRENT.backgroundColor };
      paints.add(paint);
      return { dispose: () => paints.delete(paint) };
    },
    scrollToLine: (line: number) => {
      did.push(`scroll ${line}`);
      active.viewportY = line;
      scrolled?.();
    },
    focus: () => did.push("focus"),
    onWriteParsed: (callback: () => void) => {
      parsed = callback;
      return { dispose: () => (parsed = undefined) };
    },
    onScroll: (callback: () => void) => {
      scrolled = callback;
      return { dispose: () => (scrolled = undefined) };
    },
  } as unknown as Findable;
  const lit = () => [...paints].sort((a, b) => Number(b.current) - Number(a.current)).map((p) => p.text);
  return { term, did, lines, lit, written: () => parsed?.() };
}

const box = () => screen.getByRole("searchbox", { name: "Find in opus" });
const count = () => screen.getByRole("status").textContent;

beforeEach(() => vi.useFakeTimers());
afterEach(() => vi.useRealTimers());

describe("FindBar", () => {
  it("finds every hit in the whole scrollback, starts at the newest, lights the hits near the screen and scrolls one into view only when it is out of view", () => {
    const lines = ["error: first", ...Array.from({ length: 40 }, () => "ok"), "error: second", "ok", "error: third"];
    const { term, did, lit } = fakeTerm(lines);
    render(<FindBar term={() => term} name="opus" ask={1} onClose={() => {}} />);

    fireEvent.change(box(), { target: { value: "error" } });

    expect(count()).toBe("3 of 3");
    expect(lit()).toEqual(["43:0+5", "41:0+5"]);
    expect(did).toEqual([]);

    fireEvent.keyDown(box(), { key: "Enter" });
    fireEvent.keyDown(box(), { key: "Enter" });
    expect(count()).toBe("1 of 3");
    expect(did).toEqual(["scroll 0"]);
    expect(lit()).toEqual(["0:0+5"]);
  });

  it("goes older on Enter and newer on Shift+Enter or the arrows, wrapping round", () => {
    const { term, lit } = fakeTerm(["a", "a", "a"]);
    render(<FindBar term={() => term} name="opus" ask={1} onClose={() => {}} />);
    fireEvent.change(box(), { target: { value: "a" } });

    fireEvent.keyDown(box(), { key: "Enter", shiftKey: true });
    expect(count()).toBe("1 of 3");
    fireEvent.click(screen.getByRole("button", { name: "Older match" }));
    expect(count()).toBe("3 of 3");
    fireEvent.click(screen.getByRole("button", { name: "Newer match" }));
    expect(count()).toBe("1 of 3");
    expect(lit()[0]).toBe("0:0+1");
  });

  it("says when nothing matches, and lights nothing", () => {
    const { term, lit } = fakeTerm(["hello"]);
    render(<FindBar term={() => term} name="opus" ask={1} onClose={() => {}} />);
    fireEvent.change(box(), { target: { value: "hell" } });
    expect(lit()).toEqual(["0:0+4"]);

    fireEvent.change(box(), { target: { value: "nope" } });

    expect(count()).toBe("No matches");
    expect(lit()).toEqual([]);
  });

  it("follows new output while open, keeping the hit you are on", () => {
    const fake = fakeTerm(["build error", "ok"]);
    render(<FindBar term={() => fake.term} name="opus" ask={1} onClose={() => {}} />);
    fireEvent.change(box(), { target: { value: "error" } });
    expect(count()).toBe("1 of 1");

    fake.lines.push("test error", "error again");
    act(() => fake.written());
    act(() => vi.advanceTimersByTime(300));

    expect(count()).toBe("1 of 3");
    expect(fake.lit()).toEqual(["0:6+5", "2:5+5", "3:0+5"]);
  });

  it("marks every hit along the scrollback, and a mark jumps to its hit", () => {
    const lines = ["error", ...Array.from({ length: 98 }, () => "ok"), "error"];
    const { term, did, lit } = fakeTerm(lines);
    const { container } = render(<FindBar term={() => term} name="opus" ask={1} onClose={() => {}} />);
    fireEvent.change(box(), { target: { value: "error" } });

    const shown = container.querySelectorAll<HTMLElement>("[data-mark]");
    expect([...shown].map((m) => m.style.top)).toEqual(["0%", "99%"]);
    expect(shown[1].dataset.current).toBe("true");

    fireEvent.click(shown[0]);
    expect(count()).toBe("1 of 2");
    expect(did).toEqual(["scroll 0"]);
    expect(lit()).toEqual(["0:0+5"]);
  });

  it("closes on Escape, putting out the hits and giving the terminal the keyboard back", () => {
    const { term, did, lit } = fakeTerm(["error"]);
    const onClose = vi.fn();
    render(<FindBar term={() => term} name="opus" ask={1} onClose={onClose} />);
    fireEvent.change(box(), { target: { value: "error" } });

    fireEvent.keyDown(box(), { key: "Escape" });

    expect(onClose).toHaveBeenCalled();
    expect(lit()).toEqual([]);
    expect(did).toEqual(["focus"]);
  });

  it("takes the keyboard when it opens and each time it is asked again", () => {
    const { term } = fakeTerm(["error"]);
    const view = render(<FindBar term={() => term} name="opus" ask={1} onClose={() => {}} />);
    expect(document.activeElement).toBe(box());

    box().blur();
    view.rerender(<FindBar term={() => term} name="opus" ask={2} onClose={() => {}} />);
    expect(document.activeElement).toBe(box());
  });
});
