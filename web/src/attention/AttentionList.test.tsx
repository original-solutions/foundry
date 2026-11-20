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

const overwritten: AttentionItem = {
  id: "setting_overwritten:project:p1",
  kind: "setting_overwritten",
  machine: "m2",
  machine_name: "this-mac",
  what: "Two sides of a split both edited the project foundry; the edit made on this-mac was replaced",
  todo: "Check it holds what you want, then dismiss this",
  terminal: null,
};

type Request = { url: string; method: string };

let lists: AttentionItem[][];
let dismissAnswer: () => Response;
let requests: Request[];

beforeEach(() => {
  lists = [[]];
  requests = [];
  dismissAnswer = () => new Response(null, { status: 204 });
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init: RequestInit = {}) => {
      requests.push({ url, method: init.method ?? "GET" });
      if (url === "/api/attention") return json(200, { items: lists.length > 1 ? lists.shift() : lists[0] });
      if (url.endsWith("/dismiss")) return dismissAnswer();
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
    lists = [[waiting, overwritten]];
    await renderApp(<AttentionList />);
    const list = await screen.findByRole("region", { name: "Needs attention" });
    expect(within(list).getByRole("button", { name: /2 need attention/ })).toBeTruthy();
    for (const item of [waiting, overwritten]) {
      expect(within(list).getByText(item.what)).toBeTruthy();
      expect(within(list).getByText(item.todo)).toBeTruthy();
      expect(within(list).getByText(item.machine_name)).toBeTruthy();
    }
    expect(within(list).getByText("Waiting for you")).toBeTruthy();
    expect(within(list).getByText("Setting overwritten")).toBeTruthy();
  });

  it("shows a new item within 5 seconds of it being raised", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    lists = [[], [waiting]];
    await renderApp(<AttentionList />);
    await waitFor(() => expect(reads()).toBe(1));
    expect(screen.queryByText(waiting.what)).toBeNull();
    expect(ATTENTION_EVERY).toBeLessThan(5000);
    await act(() => vi.advanceTimersByTimeAsync(ATTENTION_EVERY + 100));
    expect(await screen.findByText(waiting.what)).toBeTruthy();
  });

  it("links an item to its terminal", async () => {
    lists = [[waiting]];
    await renderApp(<AttentionList />);
    const link = await screen.findByRole("link", { name: "Open terminal" });
    expect(link.getAttribute("href")).toBe("/terminals/t1");
  });

  it("offers dismiss only on a setting overwritten item, and reads the list again after it", async () => {
    lists = [[waiting, overwritten], []];
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

  it("folds to its count and opens again", async () => {
    lists = [[waiting]];
    await renderApp(<AttentionList />);
    const toggle = await screen.findByRole("button", { name: /1 needs attention/ });
    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(screen.queryByText(waiting.what)).toBeNull();
    fireEvent.click(toggle);
    expect(screen.getByText(waiting.what)).toBeTruthy();
  });

  it("shows an item of a kind this build does not know by its kind", async () => {
    lists = [[{ ...waiting, id: "placement_waiting:r1", kind: "placement_waiting", terminal: null }]];
    await renderApp(<AttentionList />);
    expect(await screen.findByText("placement_waiting")).toBeTruthy();
  });
});
