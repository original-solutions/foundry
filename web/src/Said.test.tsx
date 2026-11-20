import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Said } from "./Said";
import { FakeSocket } from "./test-utils";

const answer = "Done. I fixed the login check.\n\nAll 12 tests pass.";

beforeEach(() => FakeSocket.install());
afterEach(() => vi.unstubAllGlobals());

const typed = (socket: FakeSocket) => socket.sent.map((d) => (typeof d === "string" ? d : new TextDecoder().decode(d)));

describe("Said", () => {
  it("shows what Claude said, a few lines until it is opened, then all of it", () => {
    render(<Said said={answer} name="opus" terminal="t1" canReply />);
    const quote = screen.getByRole("button", { name: /I fixed the login check/ });
    expect(quote.getAttribute("aria-expanded")).toBe("false");
    expect(quote.className).toContain("line-clamp-3");
    fireEvent.click(quote);
    expect(quote.getAttribute("aria-expanded")).toBe("true");
    expect(quote.className).not.toContain("line-clamp-3");
    expect(screen.getByRole("textbox", { name: "Reply to opus" })).toBeDefined();
  });

  it("types a reply into the session's terminal and presses Enter, then closes", async () => {
    render(<Said said={answer} name="opus" terminal="t1" canReply />);
    fireEvent.click(screen.getByRole("button", { name: /I fixed the login check/ }));
    const box = screen.getByRole("textbox", { name: "Reply to opus" });
    fireEvent.change(box, { target: { value: "now push it" } });
    fireEvent.submit(box);
    const socket = FakeSocket.last();
    expect(socket.url).toMatch(/\/api\/terminals\/t1$/);
    act(() => socket.open());
    expect(typed(socket)).toEqual(["\x1b[200~now push it\x1b[201~", "\r"]);
    expect(socket.readyState).toBe(FakeSocket.CLOSED);
    await waitFor(() => expect(screen.queryByRole("textbox")).toBeNull());
  });

  it("says so when the terminal cannot be reached, and keeps the reply", async () => {
    render(<Said said={answer} name="opus" terminal="t1" canReply />);
    fireEvent.click(screen.getByRole("button", { name: /I fixed the login check/ }));
    const box = screen.getByRole("textbox", { name: "Reply to opus" });
    fireEvent.change(box, { target: { value: "now push it" } });
    fireEvent.submit(box);
    act(() => FakeSocket.last().end(1011, "terminal gone"));
    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("Could not reach opus"));
    expect((screen.getByRole("textbox", { name: "Reply to opus" }) as HTMLInputElement).value).toBe("now push it");
  });

  it("sends nothing blank, offers no reply where it cannot be sent, and Escape closes", () => {
    const { rerender } = render(<Said said={answer} name="opus" terminal="t1" canReply />);
    fireEvent.click(screen.getByRole("button", { name: /I fixed the login check/ }));
    fireEvent.submit(screen.getByRole("textbox", { name: "Reply to opus" }));
    expect(FakeSocket.all).toHaveLength(0);
    fireEvent.keyDown(screen.getByRole("textbox", { name: "Reply to opus" }), { key: "Escape" });
    expect(screen.queryByRole("textbox")).toBeNull();
    rerender(<Said said={answer} name="opus" terminal="t1" canReply={false} />);
    fireEvent.click(screen.getByRole("button", { name: /I fixed the login check/ }));
    expect(screen.queryByRole("textbox")).toBeNull();
  });
});
