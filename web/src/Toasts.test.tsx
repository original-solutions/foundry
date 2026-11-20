import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { AnyRecord, Briefs, SessionState } from "./api";
import { fleet } from "./fleet";
import { MAX_TOASTS, TOAST_MS } from "./news";
import { useAlerts } from "./AlertsBell";
import { Toasts } from "./Toasts";
import { expectFieldLook } from "./test-utils";
import { ME, OTHER, sample, session, settings, terminal, workspace } from "./test-fixtures";

const sent: { id: string; text: string }[] = [];
let refuse = false;
vi.mock("./Said", () => ({
  reply: async (id: string, text: string) => {
    if (refuse) throw new Error("gone");
    sent.push({ id, text });
  },
}));

/** s1 and s2 in `fix login`, s3 in `tidy readme`, and s9 on another machine. */
function fleetAt(states: { s1?: SessionState; s2?: SessionState; s3?: SessionState; s9?: SessionState } = {}): AnyRecord[] {
  return [
    sample(ME, 1),
    workspace(ME, "w1", "fix login"),
    terminal(ME, "t1", "w1"),
    session(ME, "s1", "w1", "t1", states.s1 ?? "working"),
    terminal(ME, "t2", "w1"),
    session(ME, "s2", "w1", "t2", states.s2 ?? "working", { model: "sonnet" }),
    workspace(ME, "w2", "tidy readme"),
    terminal(ME, "t3", "w2"),
    session(ME, "s3", "w2", "t3", states.s3 ?? "working"),
    sample(OTHER, 1),
    workspace(OTHER, "w9", "far away"),
    terminal(OTHER, "t9", "w9"),
    session(OTHER, "s9", "w9", "t9", states.s9 ?? "working"),
  ];
}

const briefs: Briefs = {
  s1: { title: "add the login form", now: null, said: "The form is in.\nTests pass." },
  s2: { title: "tidy the docs", now: { kind: "ask", text: "Delete old.md?" } },
  s3: { title: "hi", now: null, said: "Hello." },
  s9: { title: "far", now: null, said: "Done over there." },
};

let visibility: DocumentVisibilityState = "visible";

beforeEach(() => {
  vi.useFakeTimers();
  Object.defineProperty(document, "visibilityState", { configurable: true, get: () => visibility });
  visibility = "visible";
  sent.length = 0;
  refuse = false;
});

afterEach(() => {
  vi.useRealTimers();
});

/** The cards as the app shows them: fed by the one hook that hears the fleet's news. */
function Both({ records, shown, covered, onOpen }: { records: AnyRecord[]; shown: string[]; covered: string[]; onOpen: (terminal: string) => void }) {
  const machines = fleet(records, settings({ machines: [] }), briefs);
  const { news } = useAlerts(machines, true, onOpen);
  return <Toasts news={news} machines={machines} here={ME} onScreen={shown} covered={covered} onOpen={onOpen} />;
}

function view(onScreen: string[] = ["t3"], covered: string[] = []) {
  const onOpen = vi.fn();
  const ui = (r: AnyRecord[], shown: string[]) => <Both records={r} shown={shown} covered={covered} onOpen={onOpen} />;
  const rendered = render(ui(fleetAt(), onScreen));
  return { onOpen, move: (r: AnyRecord[], shown = onScreen) => rendered.rerender(ui(r, shown)) };
}

const cards = () => screen.queryAllByRole("article");
const card = (name: string) => screen.getByRole("article", { name });

describe("Toasts", () => {
  it("slides in what a session said when it finishes its turn while you look at another terminal, and opens it", () => {
    const { onOpen, move } = view();
    expect(cards()).toEqual([]);

    move(fleetAt({ s1: "idle" }));
    const news = card("fix login: opus finished its turn");
    expect(within(news).getByText(/The form is in\./)).toBeDefined();
    expect(within(news).getByText("add the login form")).toBeDefined();

    fireEvent.click(within(news).getByRole("button", { name: "Open" }));
    expect(onOpen).toHaveBeenCalledWith("t1");
    expect(cards()).toEqual([]);
  });

  it("says nothing about a terminal already on screen, or while the page is hidden", () => {
    const { move } = view(["t3", "t1"]);
    move(fleetAt({ s1: "idle", s3: "idle" }));
    expect(cards()).toEqual([]);

    visibility = "hidden";
    move(fleetAt({ s1: "idle", s2: "idle", s3: "idle" }));
    expect(cards()).toEqual([]);
  });

  it("gives no card of its own to a session the catch-up already lists", () => {
    const { move } = view(["t3"], ["s1"]);
    move(fleetAt({ s1: "idle", s2: "waiting_for_you" }));
    expect(cards().map((c) => c.getAttribute("aria-label"))).toEqual(["fix login: sonnet is waiting for you"]);
  });

  it("takes a card off once its terminal is opened some other way", () => {
    const { move } = view();
    move(fleetAt({ s1: "idle" }));
    expect(cards()).toHaveLength(1);
    move(fleetAt({ s1: "idle" }), ["t1"]);
    expect(cards()).toEqual([]);
  });

  it("lets a finished turn's card go by itself, but not while the pointer rests on it", () => {
    const { move } = view();
    move(fleetAt({ s1: "idle" }));
    fireEvent.pointerEnter(card("fix login: opus finished its turn"));
    act(() => vi.advanceTimersByTime(TOAST_MS + 1000));
    expect(cards()).toHaveLength(1);

    fireEvent.pointerLeave(card("fix login: opus finished its turn"));
    act(() => vi.advanceTimersByTime(TOAST_MS - 100));
    expect(cards()).toHaveLength(1);
    act(() => vi.advanceTimersByTime(200));
    expect(cards()).toEqual([]);
  });

  it("keeps a question up, with the question, until the session moves on; it offers only Open", () => {
    const { move } = view();
    move(fleetAt({ s2: "waiting_for_you" }));
    const ask = card("fix login: sonnet is waiting for you");
    expect(within(ask).getByText("Delete old.md?")).toBeDefined();
    expect(within(ask).queryByRole("textbox")).toBeNull();
    act(() => vi.advanceTimersByTime(TOAST_MS * 5));
    expect(cards()).toHaveLength(1);

    move(fleetAt({ s2: "working" }));
    expect(cards()).toEqual([]);
  });

  it("answers Claude from the card: the reply is typed into its terminal and the card goes", async () => {
    const { move } = view();
    move(fleetAt({ s1: "idle" }));
    const news = card("fix login: opus finished its turn");
    fireEvent.change(within(news).getByRole("textbox", { name: "Reply to opus" }), { target: { value: "now the tests" } });
    await act(async () => {
      fireEvent.submit(within(news).getByRole("textbox", { name: "Reply to opus" }));
    });
    expect(sent).toEqual([{ id: "t1", text: "now the tests" }]);
    expect(cards()).toEqual([]);
  });

  it("says so when the reply cannot reach the terminal, and keeps the card", async () => {
    const { move } = view();
    move(fleetAt({ s1: "idle" }));
    refuse = true;
    const box = within(card("fix login: opus finished its turn")).getByRole("textbox", { name: "Reply to opus" });
    expectFieldLook(box);
    fireEvent.change(box, { target: { value: "hello" } });
    await act(async () => {
      fireEvent.submit(box);
    });
    expect(screen.getByRole("alert").textContent).toBe("Could not reach opus. Open its terminal to reply.");
    act(() => vi.advanceTimersByTime(TOAST_MS * 2));
    expect(cards()).toHaveLength(1);
  });

  it("offers no reply to a session on another machine", () => {
    const { move } = view();
    move(fleetAt({ s9: "idle" }));
    const news = card("far away: opus finished its turn");
    expect(within(news).queryByRole("textbox")).toBeNull();
    expect(within(news).getByRole("button", { name: "Open" })).toBeDefined();
  });

  it("puts the newest on top, one card per session, and keeps only the newest few", () => {
    const { move } = view([]);
    move(fleetAt({ s1: "idle" }));
    move(fleetAt({ s1: "idle", s2: "waiting_for_you" }));
    move(fleetAt({ s1: "idle", s2: "waiting_for_you", s3: "idle" }));
    move(fleetAt({ s1: "idle", s2: "waiting_for_you", s3: "idle", s9: "idle" }));
    expect(MAX_TOASTS).toBe(3);
    expect(cards().map((c) => c.getAttribute("aria-label"))).toEqual(["far away: opus finished its turn", "tidy readme: opus finished its turn", "fix login: sonnet is waiting for you"]);

    move(fleetAt({ s1: "idle", s2: "stuck", s3: "idle", s9: "idle" }));
    expect(cards()[0].getAttribute("aria-label")).toBe("fix login: sonnet is stuck: no output for 10 minutes");
    expect(cards()).toHaveLength(3);
  });

  it("closes a card from its × or with Escape", () => {
    const { move } = view([]);
    move(fleetAt({ s1: "idle", s3: "idle" }));
    fireEvent.click(within(card("fix login: opus finished its turn")).getByRole("button", { name: "Dismiss" }));
    expect(cards()).toHaveLength(1);
    fireEvent.keyDown(card("tidy readme: opus finished its turn"), { key: "Escape" });
    expect(cards()).toEqual([]);
  });
});
