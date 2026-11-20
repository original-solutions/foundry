import { act, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { UiMessage } from "./api";
import { settings, workspace, ME } from "./test-fixtures";
import { FakeSocket } from "./test-utils";
import { useFleet } from "./useFleet";

function Probe() {
  const { state, connected } = useFleet();
  return (
    <p>
      {connected ? "connected" : "down"} {state.records.length} records
    </p>
  );
}

const stateMessage = (records = [workspace(ME, "w1", "a")]): string =>
  JSON.stringify({ type: "state", protocol: 1, records, settings: settings() } satisfies UiMessage);

beforeEach(() => {
  FakeSocket.install();
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("useFleet", () => {
  it("opens /api/events and shows the state it is sent", () => {
    render(<Probe />);
    const socket = FakeSocket.last();
    expect(socket.url).toMatch(/\/api\/events$/);

    act(() => socket.open());
    act(() => socket.deliver(stateMessage()));

    expect(screen.getByText("connected 1 records")).toBeDefined();
  });

  it("says it is down after a close, opens again after a short wait, and takes the new state", () => {
    render(<Probe />);
    const first = FakeSocket.last();
    act(() => first.open());
    act(() => first.deliver(stateMessage()));

    act(() => first.end(1006));
    expect(screen.getByText(/^down/)).toBeDefined();
    act(() => vi.advanceTimersByTime(500));
    const second = FakeSocket.last();
    expect(second).not.toBe(first);

    act(() => second.open());
    act(() => second.deliver(stateMessage([])));
    expect(screen.getByText("connected 0 records")).toBeDefined();
  });

  it("backs off while sockets open and close before sending anything", () => {
    render(<Probe />);
    // Each socket opens, then closes before its first message, as when the daemon cannot read its store.
    for (const wait of [500, 1000, 2000]) {
      const socket = FakeSocket.last();
      act(() => socket.open());
      act(() => socket.end(1011));
      const count = FakeSocket.all.length;
      act(() => vi.advanceTimersByTime(wait - 1));
      expect(FakeSocket.all).toHaveLength(count);
      act(() => vi.advanceTimersByTime(1));
      expect(FakeSocket.all).toHaveLength(count + 1);
    }
  });

  it("waits 500 ms again after a socket that sent something closes", () => {
    render(<Probe />);
    for (let i = 0; i < 3; i++) {
      act(() => FakeSocket.last().end(1006));
      act(() => vi.advanceTimersByTime(5_000));
    }
    const socket = FakeSocket.last();
    act(() => socket.open());
    act(() => socket.deliver(stateMessage()));

    act(() => socket.end(1006));
    act(() => vi.advanceTimersByTime(500));

    expect(FakeSocket.last()).not.toBe(socket);
  });
});
