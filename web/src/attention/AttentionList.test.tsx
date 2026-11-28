import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { json, renderApp } from "../test-utils";
import type { AttentionItem } from "./attention";
import { ATTENTION_EVERY, AttentionList } from "./AttentionList";

const waiting: AttentionItem = {
  id: "session_waiting:s1",
  kind: "session_waiting",
  machine: "m1",
  machine_name: "trilogy-mac",
  what: "A session in fix login on trilogy-mac is waiting for you",
  todo: "Open its terminal and answer it",
  terminal: "t1",
  issue_url: null,
  pr_url: null,
};

const offline: AttentionItem = {
  id: "machine_offline:m3",
  kind: "machine_offline",
  machine: "m3",
  machine_name: "build-box",
  what: "build-box has not been heard from for 15 seconds",
  todo: "Check that it is on, on the tailnet and running foundryd",
  terminal: null,
  issue_url: null,
  pr_url: null,
};

const overwritten: AttentionItem = {
  id: "setting_overwritten:project:p1",
  kind: "setting_overwritten",
  machine: "m2",
  machine_name: "this-mac",
  what: "Two sides of a split both edited the project foundry; the edit made on this-mac was replaced",
  todo: "Check it holds what you want, then dismiss this",
  terminal: null,
  issue_url: null,
  pr_url: null,
};

const setupFailed: AttentionItem = {
  id: "setup_failed:w1",
  kind: "setup_failed",
  machine: "m1",
  machine_name: "trilogy-mac",
  what: "The setup of /factory-build on trilogy-mac failed (exit code 101)",
  todo: "Read its setup terminal, fix the workspace by hand, then close that terminal",
  terminal: "t9",
  issue_url: null,
  pr_url: null,
};

type Request = { url: string; method: string };

let lists: AttentionItem[][];
let dismissAnswer: (url: string) => Response;
let closeAnswer: () => Response;
let requests: Request[];

beforeEach(() => {
  lists = [[]];
  requests = [];
  dismissAnswer = () => new Response(null, { status: 204 });
  closeAnswer = () => new Response(null, { status: 204 });
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init: RequestInit = {}) => {
      requests.push({ url, method: init.method ?? "GET" });
      if (url === "/api/attention") return json(200, { items: lists.length > 1 ? lists.shift() : lists[0] });
      if (url.endsWith("/dismiss")) return dismissAnswer(url);
      if (url.endsWith("/close")) return closeAnswer();
      throw new Error(`unexpected request ${url}`);
    }),
  );
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

const reads = () => requests.filter((r) => r.url === "/api/attention").length;

describe("AttentionList", () => {
  it("shows nothing while nothing needs attention", async () => {
    const { container } = await renderApp(<AttentionList />);
    await waitFor(() => expect(reads()).toBe(1));
    expect(container.querySelector("section")).toBeNull();
  });

  it("shows each item with what happened, what to do and its machine", async () => {
    lists = [[offline, overwritten]];
    await renderApp(<AttentionList />);
    const list = await screen.findByRole("region", { name: "Needs attention" });
    expect(within(list).getByRole("button", { name: /2 need attention/ })).toBeTruthy();
    for (const item of [offline, overwritten]) {
      expect(within(list).getByText(item.what)).toBeTruthy();
      expect(within(list).getByText(item.todo)).toBeTruthy();
      expect(within(list).getByText(item.machine_name)).toBeTruthy();
    }
    expect(within(list).getByText("Machine offline")).toBeTruthy();
    expect(within(list).getByText("Setting overwritten")).toBeTruthy();
  });

  it("shows a new item within 5 seconds of it being raised", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    lists = [[], [offline]];
    await renderApp(<AttentionList />);
    await waitFor(() => expect(reads()).toBe(1));
    expect(screen.queryByText(offline.what)).toBeNull();
    expect(ATTENTION_EVERY).toBeLessThan(5000);
    await act(() => vi.advanceTimersByTimeAsync(ATTENTION_EVERY + 100));
    expect(await screen.findByText(offline.what)).toBeTruthy();
  });

  it("links an item to its terminal", async () => {
    lists = [[setupFailed]];
    await renderApp(<AttentionList />);
    const link = await screen.findByRole("link", { name: "Open terminal" });
    expect(link.getAttribute("href")).toBe("/terminals/t9");
  });

  it("leaves a session's question, stall or failure to the strip that leads to it and the board's lane, listing only the rest", async () => {
    const stuck = { ...waiting, id: "session_stuck:s2", kind: "session_stuck", terminal: "t2" };
    const failed = { ...waiting, id: "session_failed:s3", kind: "session_failed", terminal: "t3" };
    lists = [[waiting, stuck, failed, offline]];
    await renderApp(<AttentionList />);
    const list = await screen.findByRole("region", { name: "Needs attention" });
    expect(within(list).getByRole("button", { name: /1 needs attention/ })).toBeTruthy();
    expect(within(list).getByText(offline.what)).toBeTruthy();
    expect(within(list).queryByText(waiting.what)).toBeNull();
    expect(within(list).queryByRole("link", { name: "Open terminal" })).toBeNull();
  });

  it("shows nothing while only sessions need you", async () => {
    lists = [[waiting]];
    const { container } = await renderApp(<AttentionList />);
    await waitFor(() => expect(reads()).toBe(1));
    expect(container.querySelector("section")).toBeNull();
  });

  it("offers dismiss only on a setting overwritten item, and reads the list again after it", async () => {
    lists = [[offline, overwritten], []];
    await renderApp(<AttentionList />);
    const dismiss = await screen.findByRole("button", { name: "Dismiss" });
    expect(screen.getAllByRole("button", { name: "Dismiss" })).toHaveLength(1);
    fireEvent.click(dismiss);
    await waitFor(() => expect(screen.queryByRole("region", { name: "Needs attention" })).toBeNull());
    expect(requests).toContainEqual({ url: "/api/attention/setting_overwritten%3Aproject%3Ap1/dismiss", method: "POST" });
  });

  it("says why a dismissal failed", async () => {
    lists = [[overwritten]];
    dismissAnswer = () => json(503, { error: "no link to m3, the acting coordinator, is open, so the edit was not sent" });
    await renderApp(<AttentionList />);
    fireEvent.click(await screen.findByRole("button", { name: "Dismiss" }));
    expect((await screen.findByRole("alert")).textContent).toContain("no link to m3");
    expect(screen.getByText(overwritten.what)).toBeTruthy();
  });

  it("offers close terminal only on a setup failed item, and reads the list again after it", async () => {
    lists = [[offline, setupFailed], [offline]];
    await renderApp(<AttentionList />);
    const close = await screen.findByRole("button", { name: "Close terminal" });
    expect(screen.getAllByRole("button", { name: "Close terminal" })).toHaveLength(1);
    fireEvent.click(close);
    await waitFor(() => expect(screen.queryByText(setupFailed.what)).toBeNull());
    expect(requests).toContainEqual({ url: "/api/terminals/t9/close", method: "POST" });
  });

  it("says why closing a setup terminal failed", async () => {
    lists = [[setupFailed]];
    closeAnswer = () => json(503, { error: "trilogy-mac is offline" });
    await renderApp(<AttentionList />);
    fireEvent.click(await screen.findByRole("button", { name: "Close terminal" }));
    expect((await screen.findByRole("alert")).textContent).toContain("trilogy-mac is offline");
    expect(screen.getByText(setupFailed.what)).toBeTruthy();
  });

  it("offers no close on a setup failed item without a terminal", async () => {
    lists = [[{ ...setupFailed, terminal: null }]];
    await renderApp(<AttentionList />);
    await screen.findByText(setupFailed.what);
    expect(screen.queryByRole("button", { name: "Close terminal" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Dismiss" })).toBeNull();
    expect(requests.filter((request) => request.method === "POST")).toEqual([]);
  });

  it("folds to its count and opens again", async () => {
    lists = [[offline]];
    await renderApp(<AttentionList />);
    const toggle = await screen.findByRole("button", { name: /1 needs attention/ });
    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(screen.queryByText(offline.what)).toBeNull();
    fireEvent.click(toggle);
    expect(screen.getByText(offline.what)).toBeTruthy();
  });

  it("starts folded when told to, and opens to its items", async () => {
    lists = [[offline]];
    await renderApp(<AttentionList folded />);
    const toggle = await screen.findByRole("button", { name: /1 needs attention/ });
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(screen.queryByText(offline.what)).toBeNull();
    fireEvent.click(toggle);
    expect(screen.getByText(offline.what)).toBeTruthy();
  });

  it("starts folded again when the list empties and then fills, as the waiting list does", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    lists = [[offline], [], [offline]];
    await renderApp(<AttentionList folded />);
    fireEvent.click(await screen.findByRole("button", { name: /1 needs attention/ }));
    expect(screen.getByText(offline.what)).toBeTruthy();
    await act(() => vi.advanceTimersByTimeAsync(ATTENTION_EVERY + 100));
    await waitFor(() => expect(screen.queryByRole("region", { name: "Needs attention" })).toBeNull());
    await act(() => vi.advanceTimersByTimeAsync(ATTENTION_EVERY + 100));
    const toggle = await screen.findByRole("button", { name: /1 needs attention/ });
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(screen.queryByText(offline.what)).toBeNull();
  });

  it("shows an item of a kind this build does not know by its kind", async () => {
    lists = [[{ ...offline, id: "lease_lost:p1", kind: "lease_lost", terminal: null }]];
    await renderApp(<AttentionList />);
    expect(await screen.findByText("lease_lost")).toBeTruthy();
  });

  const stuck = (issue: number): AttentionItem => ({
    id: `run_stuck:tc__people-hub__${issue}__1`,
    kind: "run_stuck",
    machine: "m1",
    machine_name: "trilogy-mac",
    what: `The factory's run of tc/people-hub#${issue} is stuck: make check fails on main\nReportingEdgeTest > totals ${issue}`,
    todo: "Read the factory's last comment on the issue, fix the cause, then move the issue out of factory:stuck",
    terminal: null,
    issue_url: null,
    pr_url: null,
  });

  const issue = (n: number) => `https://github.com/tc/people-hub/issues/${n}`;
  const pull = (n: number) => `https://github.com/tc/people-hub/pull/${n}`;

  it("links a run stuck item to its issue, its pull request and its terminal, in that order", async () => {
    lists = [[{ ...stuck(7), issue_url: issue(7), pr_url: pull(70), terminal: "t7" }]];
    await renderApp(<AttentionList />);
    const links = await screen.findAllByRole("link");
    expect(links.map((link) => link.textContent)).toEqual(["Issue", "Pull request", "Open terminal"]);
    expect(links[0].getAttribute("href")).toBe(issue(7));
    expect(links[1].getAttribute("href")).toBe(pull(70));
    expect(links[2].getAttribute("href")).toBe("/terminals/t7");
    for (const link of links.slice(0, 2)) {
      expect(link.getAttribute("target")).toBe("_blank");
      expect(link.getAttribute("rel")).toBe("noreferrer");
    }
  });

  it("shows only the Issue link when the item has no pull request or terminal", async () => {
    lists = [[{ ...stuck(7), issue_url: issue(7) }]];
    await renderApp(<AttentionList />);
    expect((await screen.findByRole("link", { name: "Issue" })).getAttribute("href")).toBe(issue(7));
    expect(screen.queryByRole("link", { name: "Pull request" })).toBeNull();
    expect(screen.queryByRole("link", { name: "Open terminal" })).toBeNull();
  });

  it("shows each grouped run's own links", async () => {
    lists = [
      [
        { ...stuck(1), issue_url: issue(1), pr_url: pull(10) },
        { ...stuck(2), issue_url: issue(2) },
      ],
    ];
    await renderApp(<AttentionList />);
    const card = await screen.findByRole("listitem", { name: "2 runs stuck" });
    expect(within(card).getAllByRole("link", { name: "Issue" }).map((link) => link.getAttribute("href"))).toEqual([issue(1), issue(2)]);
    expect(within(card).getAllByRole("link", { name: "Pull request" }).map((link) => link.getAttribute("href"))).toEqual([pull(10)]);
  });

  it("collapses items of one kind into one card that counts them and says their shared todo once", async () => {
    const items = [stuck(1), stuck(2), stuck(3)];
    lists = [[offline, ...items]];
    await renderApp(<AttentionList />);
    const list = await screen.findByRole("region", { name: "Needs attention" });
    expect(within(list).getByRole("button", { name: /4 need attention/ })).toBeTruthy();
    const card = within(list).getByRole("listitem", { name: "3 runs stuck" });
    expect(within(card).getAllByText(items[0].todo)).toHaveLength(1);
    expect(within(card).getAllByText("trilogy-mac")).toHaveLength(1);
    for (const item of items) {
      expect(within(card).getByRole("button", { name: item.what.split("\n")[0] })).toBeTruthy();
      expect(within(card).queryByText(/ReportingEdgeTest/)).toBeNull();
    }
    expect(within(list).getByText(offline.what)).toBeTruthy();
    expect(within(list).getByText("Machine offline")).toBeTruthy();
  });

  it("shows a grouped item's full what when its line is opened", async () => {
    lists = [[stuck(1), stuck(2)]];
    await renderApp(<AttentionList />);
    const line = await screen.findByRole("button", { name: stuck(2).what.split("\n")[0] });
    expect(line.getAttribute("aria-expanded")).toBe("false");
    fireEvent.click(line);
    expect(line.getAttribute("aria-expanded")).toBe("true");
    expect(screen.getByText(/ReportingEdgeTest > totals 2/)).toBeTruthy();
    expect(screen.queryByText(/ReportingEdgeTest > totals 1/)).toBeNull();
  });

  it("says each grouped item's own todo and machine when they differ", async () => {
    const one = { ...stuck(1), id: "foreign_claim:p__1", kind: "foreign_claim", what: "Issue #1 is claimed", todo: "Move it out of factory:ready" };
    const two = { ...one, id: "foreign_claim:p__2", what: "Issue #2 is claimed", todo: "Move it out of factory:building", machine: "m3", machine_name: "build-box" };
    lists = [[one, two]];
    await renderApp(<AttentionList />);
    const card = await screen.findByRole("listitem", { name: "2 foreign claims" });
    expect(within(card).getByText("trilogy-mac")).toBeTruthy();
    expect(within(card).getByText("build-box")).toBeTruthy();
    expect(within(card).queryByText(one.todo)).toBeNull();
    fireEvent.click(within(card).getByRole("button", { name: two.what }));
    expect(within(card).getByText(two.todo)).toBeTruthy();
    expect(within(card).queryByText(one.todo)).toBeNull();
  });

  it("keeps each grouped item's terminal and close button", async () => {
    const other = { ...setupFailed, id: "setup_failed:w2", what: "The setup of /fix-login on trilogy-mac failed", terminal: "t8" };
    lists = [[setupFailed, other], [other]];
    await renderApp(<AttentionList />);
    const card = await screen.findByRole("listitem", { name: "2 setups failed" });
    expect(within(card).getAllByRole("link", { name: "Open terminal" }).map((link) => link.getAttribute("href"))).toEqual([
      "/terminals/t9",
      "/terminals/t8",
    ]);
    fireEvent.click(within(card).getAllByRole("button", { name: "Close terminal" })[0]);
    await waitFor(() => expect(requests).toContainEqual({ url: "/api/terminals/t9/close", method: "POST" }));
  });

  it("says why a grouped item's dismissal failed on that item's line, and keeps both lines", async () => {
    const other = { ...overwritten, id: "setting_overwritten:project:p2", what: "Two sides of a split both edited the project docs; the edit made on this-mac was replaced" };
    lists = [[overwritten, other]];
    dismissAnswer = (url) =>
      url.includes("p2") ? json(503, { error: "no link to m3, the acting coordinator, is open, so the edit was not sent" }) : new Response(null, { status: 204 });
    await renderApp(<AttentionList />);
    const card = await screen.findByRole("listitem", { name: "2 settings overwritten" });
    const line = (what: string) => within(card).getByRole("button", { name: what }).closest("li")!;
    fireEvent.click(within(line(other.what)).getByRole("button", { name: "Dismiss" }));
    const alert = await within(card).findByRole("alert");
    expect(screen.getAllByRole("alert")).toHaveLength(1);
    expect(alert.textContent).toContain("no link to m3");
    expect(line(other.what).contains(alert)).toBe(true);
    expect(line(overwritten.what).contains(alert)).toBe(false);
  });

  it("keeps a what's line breaks, alone or opened in a group", async () => {
    const claim = (issue: number): AttentionItem => ({
      ...stuck(issue),
      id: `foreign_claim:p__${issue}`,
      kind: "foreign_claim",
      what: `Issue #${issue} is claimed\nby another-factory`,
    });
    lists = [[stuck(1), claim(1), claim(2)]];
    await renderApp(<AttentionList />);
    const alone = await screen.findByText(/ReportingEdgeTest > totals 1/);
    expect(alone.className).toContain("whitespace-pre-line");
    fireEvent.click(screen.getByRole("button", { name: "Issue #2 is claimed" }));
    expect(screen.getByText(/Issue #2 is claimed\s+by another-factory/).className).toContain("whitespace-pre-line");
  });

  it("names a run stuck item", async () => {
    const one = { ...stuck(1), what: "The factory's run of tc/people-hub#1 is stuck: make check fails on main" };
    lists = [[one]];
    await renderApp(<AttentionList />);
    expect(await screen.findByText("Run stuck")).toBeTruthy();
    expect(screen.getByText(one.what)).toBeTruthy();
    expect(screen.getByText(one.todo)).toBeTruthy();
  });

  it("names a placement waiting item", async () => {
    lists = [[{ ...offline, id: "placement_waiting:r1", kind: "placement_waiting", terminal: null }]];
    await renderApp(<AttentionList />);
    expect(await screen.findByText("Placement waiting")).toBeTruthy();
  });
});
