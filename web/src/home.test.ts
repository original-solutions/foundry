import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { setHome, useHome } from "./home";

describe("the home's view", () => {
  afterEach(() => localStorage.clear());

  it("is the board until the timeline is picked, and the pick is kept in this browser", () => {
    const { result } = renderHook(() => useHome());
    expect(result.current).toBe("board");
    act(() => setHome("timeline"));
    expect(result.current).toBe("timeline");
    expect(localStorage.getItem("foundry.home")).toBe("timeline");
  });

  it("changes for every screen showing it, wherever it was picked", () => {
    const a = renderHook(() => useHome());
    const b = renderHook(() => useHome());
    act(() => setHome("timeline"));
    expect([a.result.current, b.result.current]).toEqual(["timeline", "timeline"]);
    act(() => setHome("board"));
    expect([a.result.current, b.result.current]).toEqual(["board", "board"]);
  });

  it("still changes for this page when the browser keeps nothing", () => {
    const blocked = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    const { result } = renderHook(() => useHome());
    act(() => setHome("timeline"));
    expect(result.current).toBe("timeline");
    blocked.mockRestore();
    act(() => setHome("board"));
  });
});
