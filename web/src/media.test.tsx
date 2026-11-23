import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { matches, MD, useMediaQuery } from "./media";

/** A `matchMedia` whose answer for `query` is `on()` (and `false` for any other), keeping the `change` listeners added through it. */
function stubMedia(query: string, on: () => boolean) {
  const listeners = new Set<() => void>();
  const added = vi.fn((_: string, f: () => void) => listeners.add(f));
  const removed = vi.fn((_: string, f: () => void) => listeners.delete(f));
  vi.stubGlobal("matchMedia", (asked: string) => ({
    get matches() {
      return asked === query && on();
    },
    addEventListener: added,
    removeEventListener: removed,
  }));
  return { listeners, added, removed, change: () => act(() => listeners.forEach((f) => f())) };
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("MD", () => {
  it("is the md breakpoint", () => {
    expect(MD).toBe("(min-width: 768px)");
  });
});

describe("matches", () => {
  for (const fallback of [true, false]) {
    describe(`with a fallback of ${fallback}`, () => {
      it("answers the fallback when there is no matchMedia", () => {
        vi.stubGlobal("matchMedia", undefined);
        expect(matches(MD, fallback)).toBe(fallback);
      });

      it("answers true when the query matches", () => {
        stubMedia(MD, () => true);
        expect(matches(MD, fallback)).toBe(true);
      });

      it("answers false when the query does not match", () => {
        stubMedia(MD, () => false);
        expect(matches(MD, fallback)).toBe(false);
      });

      it("asks about the query it is given", () => {
        stubMedia("(pointer: coarse)", () => true);
        expect(matches(MD, fallback)).toBe(false);
        expect(matches("(pointer: coarse)", fallback)).toBe(true);
      });
    });
  }
});

describe("useMediaQuery", () => {
  for (const fallback of [true, false]) {
    describe(`with a fallback of ${fallback}`, () => {
      it("answers the fallback when there is no matchMedia", () => {
        vi.stubGlobal("matchMedia", undefined);
        const { result } = renderHook(() => useMediaQuery(MD, fallback));
        expect(result.current).toBe(fallback);
      });

      it("answers true from its first render when the query matches", () => {
        stubMedia(MD, () => true);
        const seen: boolean[] = [];
        const { result } = renderHook(() => seen[seen.push(useMediaQuery(MD, fallback)) - 1]);
        expect(seen[0]).toBe(true);
        expect(result.current).toBe(true);
      });

      it("answers false from its first render when the query does not match", () => {
        stubMedia(MD, () => false);
        const seen: boolean[] = [];
        const { result } = renderHook(() => seen[seen.push(useMediaQuery(MD, fallback)) - 1]);
        expect(seen[0]).toBe(false);
        expect(result.current).toBe(false);
      });

      it("follows the query's change event both ways", () => {
        let on = false;
        const media = stubMedia(MD, () => on);
        const { result } = renderHook(() => useMediaQuery(MD, fallback));
        expect(result.current).toBe(false);
        on = true;
        media.change();
        expect(result.current).toBe(true);
        on = false;
        media.change();
        expect(result.current).toBe(false);
      });
    });
  }

  it("follows a new query when it is given one", () => {
    const media = stubMedia(MD, () => true);
    const { result, rerender } = renderHook(({ query }) => useMediaQuery(query, false), { initialProps: { query: "(pointer: coarse)" } });
    expect(result.current).toBe(false);
    rerender({ query: MD });
    expect(result.current).toBe(true);
    expect(media.listeners.size).toBe(1);
  });

  it("listens for the query's change event and stops on unmount", () => {
    const media = stubMedia(MD, () => true);
    const { unmount } = renderHook(() => useMediaQuery(MD, false));
    expect(media.added).toHaveBeenCalledWith("change", expect.any(Function));
    expect(media.listeners.size).toBe(1);
    unmount();
    expect(media.removed).toHaveBeenCalledWith("change", media.added.mock.calls[0][1]);
    expect(media.listeners.size).toBe(0);
  });
});
