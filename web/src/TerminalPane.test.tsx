import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { TerminalPane } from "./TerminalPane";
import type { Glyph } from "./labels";
import { FakeSocket } from "./test-utils";

// jsdom cannot run xterm.js, so a stand-in records what the pane does to its screen.
const screenOps = vi.hoisted(() => ({
  ops: [] as string[],
  typed: null as ((data: string) => void) | null,
  resized: null as (() => void) | null,
  term: null as { cols: number; rows: number } | null,
  focused: false,
}));

vi.mock("@xterm/xterm", () => ({
  Terminal: class {
    cols = 80;
    rows = 24;
    constructor() {
      screenOps.term = this;
    }
    loadAddon() {}
    open() {}
    focus() {
      screenOps.focused = true;
    }
    reset() {
      screenOps.ops.push("reset");
    }
    write(data: Uint8Array) {
      screenOps.ops.push(`write:${new TextDecoder().decode(data)}`);
    }
    onData(callback: (data: string) => void) {
      screenOps.typed = callback;
      return { dispose() {} };
    }
    onResize(callback: () => void) {
      screenOps.resized = callback;
      return { dispose() {} };
    }
    modes = { applicationCursorKeysMode: false, bracketedPasteMode: true };
    // As xterm does: a paste is typed data, wrapped when the program asked for bracketed paste.
    paste(data: string) {
      screenOps.typed?.(`\x1b[200~${data}\x1b[201~`);
    }
    input(data: string) {
      screenOps.typed?.(data);
    }
    dispose() {}
  },
}));
vi.mock("@xterm/addon-fit", () => ({ FitAddon: class { fit() {} } }));

const bytes = (text: string) => new TextEncoder().encode(text).buffer;

beforeEach(() => {
  screenOps.ops = [];
  screenOps.typed = null;
  screenOps.resized = null;
  screenOps.term = null;
  screenOps.focused = false;
  FakeSocket.install();
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

// An options object, not a defaulted parameter: a default would turn an explicit `undefined` into `true`.
function mount(id = "t1", { running, glyph }: { running: boolean | undefined; glyph?: Glyph } = { running: true }) {
  const view = render(<TerminalPane id={id} running={running} name="opus" glyph={glyph} />);
  return { socket: FakeSocket.last(), view };
}

describe("TerminalPane", () => {
  it("takes the keyboard when it opens, so a picked terminal can be typed into at once", () => {
    mount();
    expect(screenOps.focused).toBe(true);
  });

  it("attaches to the terminal's socket, sends its size, and writes the kept screen over a reset", () => {
    const { socket } = mount("t 1/x");
    expect(socket.url).toMatch(/\/api\/terminals\/t%201%2Fx$/);

    act(() => socket.open());
    expect(socket.sent).toEqual([JSON.stringify({ type: "resize", cols: 80, rows: 24 })]);

    act(() => socket.deliver(bytes("kept screen")));
    act(() => socket.deliver(bytes("live")));
    expect(screenOps.ops).toEqual(["reset", "write:kept screen", "write:live"]);
  });

  it("sends typed keys as bytes", () => {
    const { socket } = mount();
    act(() => socket.open());

    act(() => screenOps.typed?.("ls\r"));

    expect(new TextDecoder().decode(socket.sent.at(-1) as Uint8Array)).toBe("ls\r");
  });

  it("attaches again after a 1013 and resets for the screen it is sent again", () => {
    const { socket: first } = mount();
    act(() => first.open());
    act(() => first.deliver(bytes("a")));

    act(() => first.end(1013, "fell behind"));
    act(() => vi.advanceTimersByTime(1000));
    const second = FakeSocket.last();
    expect(second).not.toBe(first);
    act(() => second.open());
    act(() => second.deliver(bytes("a again")));

    expect(screenOps.ops).toEqual(["reset", "write:a", "reset", "write:a again"]);
  });

  it("says the terminal exited on 1000 and does not attach again", () => {
    const { socket } = mount();
    act(() => socket.open());

    act(() => socket.end(1000, "exited"));
    act(() => vi.advanceTimersByTime(60_000));

    expect(screen.getByText(/exited/i)).toBeDefined();
    expect(FakeSocket.all).toHaveLength(1);
  });

  it("shows the reason of a 1011 and does not attach again", () => {
    const { socket } = mount();
    act(() => socket.open());

    act(() => socket.end(1011, "holder gone"));
    act(() => vi.advanceTimersByTime(60_000));

    expect(screen.getByText("holder gone")).toBeDefined();
    expect(FakeSocket.all).toHaveLength(1);
  });

  it("reconnects after the socket drops, as when the daemon restarts, and says so meanwhile", () => {
    const { socket } = mount();
    act(() => socket.open());

    act(() => socket.end(1006));
    expect(screen.getByText(/reconnecting/i)).toBeDefined();
    act(() => vi.advanceTimersByTime(10_000));
    const again = FakeSocket.last();
    expect(again).not.toBe(socket);

    act(() => again.open());
    expect(screen.queryByText(/reconnecting/i)).toBeNull();
  });

  it("keeps trying a terminal it cannot attach yet, and says so", () => {
    const { socket } = mount();

    act(() => socket.end(1006));
    expect(screen.getByText(/not attached yet/i)).toBeDefined();
    act(() => vi.advanceTimersByTime(10_000));

    expect(FakeSocket.all.length).toBeGreaterThan(1);
  });

  it("shows input the terminal refused", () => {
    const { socket } = mount();
    act(() => socket.open());

    act(() => socket.deliver(JSON.stringify({ type: "error", error: "terminal has exited" })));

    expect(screen.getByText("terminal has exited")).toBeDefined();
  });

  it("keeps reconnecting after a drop for as long as its terminal is listed as running", () => {
    const { socket } = mount();
    act(() => socket.open());
    act(() => socket.end(1006));

    // The daemon is down for a minute: every attach fails before it opens.
    for (let second = 0; second < 60; second += 5) {
      act(() => vi.advanceTimersByTime(5_000));
      act(() => FakeSocket.last().end(1006));
    }
    const count = FakeSocket.all.length;
    act(() => vi.advanceTimersByTime(5_000));

    expect(FakeSocket.all.length).toBeGreaterThan(count);
    expect(screen.getByText(/reconnecting/i)).toBeDefined();
  });

  it("stops trying once its terminal is no longer listed as running, and says so", () => {
    const { socket, view } = mount();
    act(() => socket.open());

    view.rerender(<TerminalPane id="t1" running={false} />);
    act(() => socket.end(1013));
    act(() => vi.advanceTimersByTime(60_000));

    expect(FakeSocket.all).toHaveLength(1);
    expect(screen.getByText(/no longer running/i)).toBeDefined();
  });

  it("says a terminal it never attached is not available here after about ten seconds, and keeps trying every 5 s", () => {
    mount();

    for (let i = 0; i < 20; i++) {
      act(() => FakeSocket.last().end(1006));
      act(() => vi.advanceTimersByTime(5_000));
    }
    expect(screen.getByText(/not available here/i)).toBeDefined();
    const count = FakeSocket.all.length;
    act(() => FakeSocket.last().end(1006));
    act(() => vi.advanceTimersByTime(5_000));

    expect(FakeSocket.all).toHaveLength(count + 1);
  });

  it("sends the new size when the terminal is resized", () => {
    const { socket } = mount();
    act(() => socket.open());

    screenOps.term!.cols = 120;
    screenOps.term!.rows = 40;
    act(() => screenOps.resized?.());

    expect(socket.sent.at(-1)).toBe(JSON.stringify({ type: "resize", cols: 120, rows: 40 }));
  });

  it("keeps reattaching after a drop while the state has not synced yet", () => {
    const { socket } = mount("t1", { running: undefined });
    act(() => socket.open());

    act(() => socket.end(1006));
    act(() => vi.advanceTimersByTime(5_000));
    const second = FakeSocket.last();
    expect(second).not.toBe(socket);
    act(() => second.open());

    act(() => second.end(1013));
    act(() => vi.advanceTimersByTime(5_000));
    const third = FakeSocket.last();
    expect(third).not.toBe(second);
    act(() => third.open());

    expect(screen.queryByText(/no longer running/i)).toBeNull();
  });

  it("attaches once the daemon is back, however long it was down, while its terminal is listed as running", () => {
    mount();
    // The daemon restarts for two minutes: every attach fails before it opens.
    for (let i = 0; i < 24; i++) {
      act(() => FakeSocket.last().end(1006));
      act(() => vi.advanceTimersByTime(5_000));
    }
    expect(screen.getByText(/not available here/i)).toBeDefined();

    act(() => FakeSocket.last().open());

    expect(screen.queryByText(/not available here/i)).toBeNull();
  });

  it("stops trying a terminal it never attached once the state no longer lists it as running", () => {
    const { view } = mount();
    act(() => FakeSocket.last().end(1006));

    view.rerender(<TerminalPane id="t1" running={false} />);
    const count = FakeSocket.all.length;
    act(() => vi.advanceTimersByTime(60_000));

    expect(FakeSocket.all).toHaveLength(count);
    expect(screen.getByText(/no longer running/i)).toBeDefined();
  });

  it("backs off a client the holder cuts off again and again", () => {
    mount();
    // Each attach opens, gets the kept screen, and is cut off with 1013 at once.
    for (let step = 0; step < 100; step++) {
      const socket = FakeSocket.last();
      if (socket.readyState === FakeSocket.CONNECTING) {
        act(() => socket.open());
        act(() => socket.deliver(bytes("screen")));
        act(() => socket.end(1013));
      }
      act(() => vi.advanceTimersByTime(100));
    }

    // Ten seconds at a fixed 200 ms would be about 50 attaches.
    expect(FakeSocket.all.length).toBeLessThan(10);
  });

  it("reattaches quickly after a drop that ends a long, steady connection", () => {
    const { socket } = mount();
    act(() => socket.open());
    // Two quick drops grow the wait.
    for (let i = 0; i < 2; i++) {
      act(() => FakeSocket.last().end(1006));
      act(() => vi.advanceTimersByTime(5_000));
      act(() => FakeSocket.last().open());
    }
    act(() => FakeSocket.last().deliver(bytes("screen")));
    act(() => vi.advanceTimersByTime(60_000));
    const steady = FakeSocket.last();

    act(() => steady.end(1006));
    act(() => vi.advanceTimersByTime(600));

    expect(FakeSocket.last()).not.toBe(steady);
  });

  it("leaves the old terminal alone once the selection changes, even when its close arrives late", () => {
    const { socket: old, view } = mount("t1");
    act(() => old.end(1006));

    view.rerender(<TerminalPane id="t2" running />);
    const next = FakeSocket.last();
    expect(next.url).toMatch(/\/api\/terminals\/t2$/);
    act(() => next.open());
    // FakeSocket.close() fires nothing, so fire the close a browser would send after the cleanup's close().
    act(() => old.end(1005));
    act(() => vi.advanceTimersByTime(60_000));

    expect(FakeSocket.all.filter((s) => /\/t1$/.test(s.url))).toHaveLength(1);
    expect(screen.queryByText(/reconnecting|trying again/i)).toBeNull();
  });

  it("closes the old socket and attaches the new terminal when the selection changes", () => {
    const { socket, view } = mount("t1");
    act(() => socket.open());

    view.rerender(<TerminalPane id="t2" running />);

    expect(socket.readyState).toBe(FakeSocket.CLOSED);
    expect(FakeSocket.last().url).toMatch(/\/api\/terminals\/t2$/);
  });

  describe("touch keys", () => {
    const sent = (socket: FakeSocket) => socket.sent.filter((d) => typeof d !== "string").map((d) => new TextDecoder().decode(d as Uint8Array));

    it("sends the keys a phone keyboard lacks, as a keyboard would", () => {
      const { socket } = mount();
      act(() => socket.open());

      fireEvent.click(screen.getByRole("button", { name: "Escape" }));
      fireEvent.click(screen.getByRole("button", { name: "Up" }));
      fireEvent.click(screen.getByRole("button", { name: "Shift+Tab" }));

      expect(sent(socket)).toEqual(["\x1b", "\x1b[A", "\x1b[Z"]);
    });

    it("sends a written message as one paste and then Enter, and clears the box", () => {
      const { socket } = mount();
      act(() => socket.open());

      const box = screen.getByRole("textbox", { name: "Message opus" });
      fireEvent.change(box, { target: { value: "say hi" } });
      fireEvent.click(screen.getByRole("button", { name: "Send" }));

      expect(sent(socket)).toEqual(["\x1b[200~say hi\x1b[201~", "\r"]);
      expect((box as HTMLInputElement).value).toBe("");
    });

    it("sends on Enter in the box, and sends nothing for an empty box", () => {
      const { socket } = mount();
      act(() => socket.open());

      const box = screen.getByRole("textbox", { name: "Message opus" });
      fireEvent.keyDown(box, { key: "Enter" });
      expect(sent(socket)).toEqual([]);
      fireEvent.change(box, { target: { value: "ls" } });
      fireEvent.keyDown(box, { key: "Enter" });

      expect(sent(socket)).toEqual(["\x1b[200~ls\x1b[201~", "\r"]);
    });

    it("offers one-tap answers while the session waits for you", () => {
      const { socket } = mount("t1", { running: true, glyph: "waiting" });
      act(() => socket.open());

      fireEvent.click(screen.getByRole("button", { name: "Answer 1" }));

      expect(sent(socket)).toEqual(["1"]);
    });

    it("offers no answers while the session works", () => {
      mount("t1", { running: true, glyph: "working" });
      expect(screen.queryByRole("button", { name: "Answer 1" })).toBeNull();
    });

    it("goes away once the terminal is no longer running", () => {
      mount("t1", { running: false });
      expect(screen.queryByRole("toolbar")).toBeNull();
    });

    it("does not raise a touch keyboard over the screen when a terminal opens", () => {
      vi.stubGlobal("matchMedia", (query: string) => ({ matches: query === "(pointer: coarse)" }));
      mount();
      expect(screenOps.focused).toBe(false);
    });
  });
});
