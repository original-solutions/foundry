import { act, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { TerminalPane } from "./TerminalPane";
import { FakeSocket } from "./test-utils";

// jsdom cannot run xterm.js, so a stand-in records what the pane does to its screen.
const screenOps = vi.hoisted(() => ({
  ops: [] as string[],
  typed: null as ((data: string) => void) | null,
  resized: null as (() => void) | null,
  term: null as { cols: number; rows: number } | null,
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
function mount(id = "t1", { running }: { running: boolean | undefined } = { running: true }) {
  const view = render(<TerminalPane id={id} running={running} />);
  return { socket: FakeSocket.last(), view };
}

describe("TerminalPane", () => {
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

  it("gives up on a terminal it never attached to after about ten seconds", () => {
    mount();

    for (let i = 0; i < 20; i++) {
      act(() => FakeSocket.last().end(1006));
      act(() => vi.advanceTimersByTime(5_000));
    }
    const count = FakeSocket.all.length;
    act(() => vi.advanceTimersByTime(60_000));

    expect(FakeSocket.all).toHaveLength(count);
    expect(count).toBeLessThan(10);
    expect(screen.getByText(/not available here/i)).toBeDefined();
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

  it("tries again after giving up once the state lists its terminal as running", () => {
    const { view } = mount("t1", { running: undefined });
    for (let i = 0; i < 20; i++) {
      act(() => FakeSocket.last().end(1006));
      act(() => vi.advanceTimersByTime(5_000));
    }
    expect(screen.getByText(/not available here/i)).toBeDefined();
    const count = FakeSocket.all.length;

    view.rerender(<TerminalPane id="t1" running />);

    expect(FakeSocket.all.length).toBe(count + 1);
    act(() => FakeSocket.last().open());
    expect(screen.queryByText(/not available here/i)).toBeNull();
  });

  it("does not try again after giving up when the state already listed its terminal as running", () => {
    const { view } = mount("t1", { running: true });
    for (let i = 0; i < 20; i++) {
      act(() => FakeSocket.last().end(1006));
      act(() => vi.advanceTimersByTime(5_000));
    }
    const count = FakeSocket.all.length;

    view.rerender(<TerminalPane id="t1" running />);
    act(() => vi.advanceTimersByTime(60_000));

    expect(FakeSocket.all).toHaveLength(count);
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

  it("closes the old socket and attaches the new terminal when the selection changes", () => {
    const { socket, view } = mount("t1");
    act(() => socket.open());

    view.rerender(<TerminalPane id="t2" running />);

    expect(socket.readyState).toBe(FakeSocket.CLOSED);
    expect(FakeSocket.last().url).toMatch(/\/api\/terminals\/t2$/);
  });
});
