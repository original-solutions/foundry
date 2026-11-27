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
};

const offline: AttentionItem = {
  id: "machine_offline:m3",
  kind: "machine_offline",
  machine: "m3",
  machine_name: "build-box",
  what: "build-box has not been heard from for 15 seconds",
  todo: "Check that it is on, on the tailnet and running foundryd",
  terminal: null,
};

const overwritten: AttentionItem = {
  id: "setting_overwritten:project:p1",
  kind: "setting_overwritten",
  machine: "m2",
  machine_name: "this-mac",
  what: "Two sides of a split both edited the project foundry; the edit made on this-mac was replaced",
  todo: "Check it holds what you want, then dismiss this",
  terminal: null,
};

const setupFailed: AttentionItem = {
  id: "setup_failed:w1",
  kind: "setup_failed",
  machine: "m1",
  machine_name: "trilogy-mac",
  what: "The setup of /factory-build on trilogy-mac failed (exit code 101)",
  todo: "Read its setup terminal, fix the workspace by hand, then close that terminal",
  terminal: "t9",
};

type Request = { url: string; method: string };

let lists: AttentionItem[][];
let dismissAnswer: () => Response;
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
      if (url.endsWith("/dismiss")) return dismissAnswer();
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

  it("names a placement waiting item", async () => {
    lists = [[{ ...offline, id: "placement_waiting:r1", kind: "placement_waiting", terminal: null }]];
    await renderApp(<AttentionList />);
    expect(await screen.findByText("Placement waiting")).toBeTruthy();
  });
});
