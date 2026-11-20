import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LinkBanner, Unreachable } from "./Connection";
import type { Link } from "./useFleet";

const T0 = new Date("2026-11-20T09:00:00Z").getTime();

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(T0);
});
afterEach(() => vi.useRealTimers());

const link = (over: Partial<Link> = {}): Link => ({ tries: 0, retry: vi.fn(), ...over });

describe("Unreachable", () => {
  it("says the daemon cannot be reached, where it was looked for, how often it was tried, and counts down to the next try", () => {
    const retry = vi.fn();
    render(<Unreachable link={link({ lostAt: T0 - 20_000, retryAt: T0 + 2_000, tries: 3, retry })} />);

    expect(screen.getByRole("heading", { name: "Can't reach the daemon" })).toBeDefined();
    expect(screen.getByText(location.host)).toBeDefined();
    expect(screen.getByText("Tried 3 times over 20 s")).toBeDefined();
    expect(screen.getByText("Next try in 2 s")).toBeDefined();

    act(() => vi.advanceTimersByTime(1000));
    expect(screen.getByText("Next try in 1 s")).toBeDefined();
    act(() => vi.advanceTimersByTime(1000));
    expect(screen.getByText("Trying…")).toBeDefined();

    fireEvent.click(screen.getByRole("button", { name: "Try now" }));
    expect(retry).toHaveBeenCalled();
  });

  it("says it is trying while a try is under way", () => {
    render(<Unreachable link={link({ lostAt: T0, tries: 1 })} />);
    expect(screen.getByText("Trying…")).toBeDefined();
    expect(screen.getByText(/Tried once/)).toBeDefined();
  });
});

describe("LinkBanner", () => {
  it("shows nothing while the link is live", () => {
    const { container } = render(<LinkBanner link={link()} />);
    expect(container.textContent).toBe("");
  });

  it("says since when the page shows the daemon's last word, and offers to try now", () => {
    const retry = vi.fn();
    render(<LinkBanner link={link({ lostAt: T0 - 12_000, retryAt: T0 + 3_000, tries: 4, retry })} />);

    const banner = screen.getByRole("status");
    expect(banner.textContent).toContain("Lost the daemon 12 s ago");
    expect(banner.textContent).toContain("what it last said");
    expect(banner.textContent).toContain("Retrying in 3 s");
    fireEvent.click(screen.getByRole("button", { name: "Try now" }));
    expect(retry).toHaveBeenCalled();
  });

  it("says once that it is back and for how long it was gone, then goes", () => {
    const { rerender } = render(<LinkBanner link={link({ lostAt: T0, retryAt: T0 + 500, tries: 1 })} />);
    rerender(<LinkBanner link={link({ back: { after: 14_000, at: T0 + 14_000 } })} />);

    expect(screen.getByRole("status").textContent).toContain("Back after 14 s");
    act(() => vi.advanceTimersByTime(4_000));
    expect(screen.queryByRole("status")).toBeNull();
  });
});
