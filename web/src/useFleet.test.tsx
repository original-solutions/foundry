import { act, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { UiMessage } from "./api";
import { sample, settings, workspace, ME } from "./test-fixtures";
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

function SamplesProbe() {
  const { samples } = useFleet();
  const trace = samples[ME];
  return <p>{trace ? `${trace.points.length} points, ${trace.heard === undefined ? "not heard" : "heard"}` : "no samples"}</p>;
}

function LinkProbe() {
  const { link } = useFleet();
  return (
    <>
      <p>{link.lostAt === undefined ? `live, ${link.tries} tries${link.back ? `, back after ${link.back.after}` : ""}` : `lost at ${link.lostAt}, next at ${link.retryAt ?? "none"}, ${link.tries} tries`}</p>
      <button type="button" onClick={link.retry}>
        retry
      </button>
    </>
  );
}

describe("useFleet", () => {
  it("keeps each machine's samples as they arrive, and a newer one than the first means it was heard", () => {
    render(<SamplesProbe />);
    const socket = FakeSocket.last();
    act(() => socket.open());
    act(() => socket.deliver(stateMessage([sample(ME, 3)])));
    expect(screen.getByText("1 points, not heard")).toBeDefined();
    act(() => socket.deliver(JSON.stringify({ type: "record", protocol: 1, record: sample(ME, 4) } satisfies UiMessage)));
    expect(screen.getByText("2 points, heard")).toBeDefined();
  });

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

  it("keeps when the link was lost across failed tries, when it tries next, and clears both once the daemon sends something", () => {
    render(<LinkProbe />);
    const first = FakeSocket.last();
    act(() => first.open());
    act(() => first.deliver(stateMessage()));
    expect(screen.getByText("live, 0 tries")).toBeDefined();

    const lost = Date.now();
    act(() => first.end(1006));
    expect(screen.getByText(`lost at ${lost}, next at ${lost + 500}, 1 tries`)).toBeDefined();

    act(() => vi.advanceTimersByTime(500));
    // This one opens and closes before sending anything: still lost since the first close.
    act(() => FakeSocket.last().open());
    act(() => FakeSocket.last().end(1011));
    expect(screen.getByText(`lost at ${lost}, next at ${Date.now() + 1000}, 2 tries`)).toBeDefined();

    act(() => vi.advanceTimersByTime(1000));
    act(() => FakeSocket.last().open());
    act(() => FakeSocket.last().deliver(stateMessage()));
    expect(screen.getByText("live, 0 tries, back after 1500")).toBeDefined();
  });

  it("tries again at once when asked, instead of waiting out the backoff", () => {
    render(<LinkProbe />);
    act(() => FakeSocket.last().end(1006));
    act(() => vi.advanceTimersByTime(500));
    act(() => FakeSocket.last().end(1006));
    const count = FakeSocket.all.length;

    act(() => screen.getByRole("button", { name: "retry" }).click());
    expect(FakeSocket.all).toHaveLength(count + 1);
    expect(screen.getByText(/next at none/)).toBeDefined();
    // The wait it skipped does not open a second one.
    act(() => vi.advanceTimersByTime(1000));
    expect(FakeSocket.all).toHaveLength(count + 1);
    // A try already under way is not doubled.
    act(() => screen.getByRole("button", { name: "retry" }).click());
    expect(FakeSocket.all).toHaveLength(count + 1);
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
