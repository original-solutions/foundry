import { act, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { TerminalPane } from "./TerminalPane";
import { FakeSocket } from "./test-utils";

// jsdom cannot run xterm.js, so a stand-in records what the pane does to its screen.
const screenOps = vi.hoisted(() => ({ ops: [] as string[], typed: null as ((data: string) => void) | null }));

vi.mock("@xterm/xterm", () => ({
  Terminal: class {
    cols = 80;
    rows = 24;
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
    onResize() {
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

function mount(id = "t1") {
  render(<TerminalPane id={id} />);
  return FakeSocket.last();
}

describe("TerminalPane", () => {
  it("attaches to the terminal's socket, sends its size, and writes the kept screen over a reset", () => {
    const socket = mount("t 1/x");
    expect(socket.url).toMatch(/\/api\/terminals\/t%201%2Fx$/);

    act(() => socket.open());
    expect(socket.sent).toEqual([JSON.stringify({ type: "resize", cols: 80, rows: 24 })]);

    act(() => socket.deliver(bytes("kept screen")));
    act(() => socket.deliver(bytes("live")));
    expect(screenOps.ops).toEqual(["reset", "write:kept screen", "write:live"]);
  });

  it("sends typed keys as bytes", () => {
    const socket = mount();
    act(() => socket.open());

    act(() => screenOps.typed?.("ls\r"));

    expect(new TextDecoder().decode(socket.sent.at(-1) as Uint8Array)).toBe("ls\r");
  });

  it("attaches again after a 1013 and resets for the screen it is sent again", () => {
    const first = mount();
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
    const socket = mount();
    act(() => socket.open());

    act(() => socket.end(1000, "exited"));
    act(() => vi.advanceTimersByTime(60_000));

    expect(screen.getByText(/exited/i)).toBeDefined();
    expect(FakeSocket.all).toHaveLength(1);
  });

  it("shows the reason of a 1011 and does not attach again", () => {
    const socket = mount();
    act(() => socket.open());

    act(() => socket.end(1011, "holder gone"));
    act(() => vi.advanceTimersByTime(60_000));

    expect(screen.getByText("holder gone")).toBeDefined();
    expect(FakeSocket.all).toHaveLength(1);
  });

  it("reconnects after the socket drops, as when the daemon restarts, and says so meanwhile", () => {
    const socket = mount();
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
    const socket = mount();

    act(() => socket.end(1006));
    expect(screen.getByText(/not attached yet/i)).toBeDefined();
    act(() => vi.advanceTimersByTime(10_000));

    expect(FakeSocket.all.length).toBeGreaterThan(1);
  });

  it("shows input the terminal refused", () => {
    const socket = mount();
    act(() => socket.open());

    act(() => socket.deliver(JSON.stringify({ type: "error", error: "terminal has exited" })));

    expect(screen.getByText("terminal has exited")).toBeDefined();
  });
});
