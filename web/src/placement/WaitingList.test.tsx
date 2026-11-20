import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Placement } from "../api";
import { json, renderApp } from "../test-utils";
import { WaitingList } from "./WaitingList";

const waiting: Placement = {
  id: "r1",
  project: "p1",
  name: "fix login",
  state: "waiting",
  waiting_since: Date.now() - 3 * 60_000,
  reasons: ["studio: load 1.8 over limit 1.0", "laptop: offline"],
  error: null,
};

const failed: Placement = {
  id: "r2",
  project: "p1",
  name: "docs",
  state: "failed",
  waiting_since: Date.now() - 60_000,
  reasons: [],
  error: "docs could not be made on laptop: the branch docs already exists",
};

type Request = { url: string; method: string };

let lists: Placement[][];
let cancelAnswer: () => Response;
let requests: Request[];

beforeEach(() => {
  lists = [[]];
  requests = [];
  cancelAnswer = () => new Response(null, { status: 204 });
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init: RequestInit = {}) => {
      requests.push({ url, method: init.method ?? "GET" });
      if (url === "/api/placements") return json(200, { requests: lists.length > 1 ? lists.shift() : lists[0] });
      if (url.endsWith("/cancel")) return cancelAnswer();
      throw new Error(`unexpected request ${url}`);
    }),
  );
});

afterEach(() => vi.unstubAllGlobals());

const reads = () => requests.filter((r) => r.url === "/api/placements").length;

describe("WaitingList", () => {
  it("shows nothing while no request waits", async () => {
    const { container } = await renderApp(<WaitingList />);
    await waitFor(() => expect(reads()).toBe(1));
    expect(container.querySelector("section")).toBeNull();
  });

  it("shows each waiting request with each machine's reason and how long it has waited", async () => {
    lists = [[waiting]];
    await renderApp(<WaitingList />);
    const list = await screen.findByRole("region", { name: "Waiting for a machine" });
    expect(within(list).getByText("fix login")).toBeTruthy();
    expect(within(list).getByText("waiting 3 min")).toBeTruthy();
    for (const reason of waiting.reasons) expect(within(list).getByText(reason)).toBeTruthy();
    expect(within(list).getByRole("button", { name: "Cancel fix login" })).toBeTruthy();
  });

  it("cancels a request and drops it from the list", async () => {
    lists = [[waiting], []];
    await renderApp(<WaitingList />);
    fireEvent.click(await screen.findByRole("button", { name: "Cancel fix login" }));
    await waitFor(() => expect(requests).toContainEqual({ url: "/api/placements/r1/cancel", method: "POST" }));
    await waitFor(() => expect(screen.queryByRole("region", { name: "Waiting for a machine" })).toBeNull());
  });

  it("shows why a placement failed, and dismisses it through the same cancel", async () => {
    lists = [[failed], []];
    await renderApp(<WaitingList />);
    const list = await screen.findByRole("region", { name: "Waiting for a machine" });
    expect(within(list).getByText(failed.error!)).toBeTruthy();
    fireEvent.click(within(list).getByRole("button", { name: "Dismiss docs" }));
    await waitFor(() => expect(requests).toContainEqual({ url: "/api/placements/r2/cancel", method: "POST" }));
  });

  it("shows the daemon's words when a cancel is refused", async () => {
    lists = [[waiting]];
    cancelAnswer = () => json(409, { error: "request r1 is being placed now, so it can no longer be cancelled" });
    await renderApp(<WaitingList />);
    fireEvent.click(await screen.findByRole("button", { name: "Cancel fix login" }));
    expect((await screen.findByRole("alert")).textContent).toBe("request r1 is being placed now, so it can no longer be cancelled");
  });
});
