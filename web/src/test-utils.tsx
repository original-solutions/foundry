import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, RouterProvider } from "@tanstack/react-router";
import { render } from "@testing-library/react";
import type { ReactNode } from "react";
import { expect, vi } from "vitest";
import { emptyState } from "./fleet";
import { buttonShape, fieldLook } from "./Modal";
import type { PageProps } from "./SettingsPage";

/** The props every settings page takes: a synced empty fleet, no machines, and the default notify choices. Each can be replaced. */
export function pageProps(over: Partial<PageProps> = {}): PageProps {
  return {
    state: { ...emptyState, synced: true },
    machines: [],
    here: undefined,
    notify: { notify: { cards: true, desktop: false, away: true }, desktop: "ask", onChoose: () => {} },
    ...over,
  };
}

/** Renders `ui` inside a query client and a router that knows the terminal and settings routes, so links resolve. Gives back the query client too. */
export async function renderApp(ui: ReactNode) {
  const root = createRootRoute({ component: () => <>{ui}</> });
  const terminal = createRoute({ getParentRoute: () => root, path: "/terminals/$terminalId" });
  const settings = createRoute({ getParentRoute: () => root, path: "/settings" });
  const category = createRoute({ getParentRoute: () => root, path: "/settings/$category" });
  const machine = createRoute({ getParentRoute: () => root, path: "/settings/machines/$machine" });
  const router = createRouter({
    routeTree: root.addChildren([terminal, settings, category, machine]),
    history: createMemoryHistory({ initialEntries: ["/"] }),
  });
  await router.load();
  const client = new QueryClient({ defaultOptions: { mutations: { retry: false } } });
  const view = render(
    <QueryClientProvider client={client}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  return { ...view, client };
}

export type Call = { url: string; method: string; body: unknown };

/**
 * Fails unless `element` is built from the shared button's shape, and holds one set of colours:
 * the shared ones, or its own tone and none of the shared ones.
 */
export function expectButtonShape(element: HTMLElement) {
  const classes = element.className.split(" ");
  expect(classes).toEqual(expect.arrayContaining(buttonShape.split(" ")));
  const borders = classes.filter((c) => /^border-(?!0|2|4)[a-z]/.test(c));
  const hovers = classes.filter((c) => c.startsWith("hover:bg-"));
  expect(borders.length, element.className).toBeLessThanOrEqual(1);
  expect(hovers.length, element.className).toBeLessThanOrEqual(1);
}

/** Fails unless `element` is built from the shared field look, so it keeps its focus border and the browser's focus ring. */
export function expectFieldLook(element: HTMLElement) {
  const classes = element.className.split(" ");
  expect(classes).toEqual(expect.arrayContaining(fieldLook.split(" ")));
  expect(classes).not.toContain("focus:outline-none");
}

/**
 * Stubs `fetch` to give `answers` in order, and returns the requests it was sent. The layout's reads
 * of session briefs, the attention list and the waiting list get none, a read of where a workspace's branch
 * stands gets a branch never pushed with nothing to push (so no line shows), and the task box's
 * read of the machines gets this machine alone; all are left out, so a test sees only the
 * requests it makes.
 */
export function mockFetch(...answers: Response[]): Call[] {
  const calls: Call[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init: RequestInit = {}) => {
      if (url === "/api/briefs") return json(200, {});
      if (url === "/api/attention") return json(200, { items: [] });
      if (url === "/api/placements") return json(200, { requests: [] });
      if (url === "/api/machines") return json(200, { me: "me", machines: [] });
      if (/^\/api\/workspaces\/[^/]+\/pr$/.test(url)) return json(200, { branch: "b", pushed: false, unpushed: 0, pr: null, unreachable: false });
      calls.push({ url, method: init.method ?? "GET", body: init.body ? JSON.parse(String(init.body)) : undefined });
      const next = answers.shift();
      if (!next) throw new Error("unexpected request");
      return next;
    }),
  );
  return calls;
}

export const json = (status: number, body: unknown) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

/** A WebSocket the test drives: it records what the code sends and opens, delivers and closes on demand. */
export class FakeSocket {
  static all: FakeSocket[] = [];
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;

  readyState = FakeSocket.CONNECTING;
  binaryType = "blob";
  sent: (string | Uint8Array)[] = [];
  onopen: (() => void) | null = null;
  onmessage: ((event: { data: unknown }) => void) | null = null;
  onclose: ((event: { code: number; reason: string }) => void) | null = null;
  readonly url: string;

  constructor(url: string) {
    this.url = url;
    FakeSocket.all.push(this);
  }

  send(data: string | Uint8Array) {
    this.sent.push(data);
  }

  close() {
    this.readyState = FakeSocket.CLOSED;
  }

  open() {
    this.readyState = FakeSocket.OPEN;
    this.onopen?.();
  }

  deliver(data: string | ArrayBuffer) {
    this.onmessage?.({ data });
  }

  /** The server or the network ends the socket with `code`. */
  end(code: number, reason = "") {
    this.readyState = FakeSocket.CLOSED;
    this.onclose?.({ code, reason });
  }

  static install() {
    FakeSocket.all = [];
    vi.stubGlobal("WebSocket", FakeSocket);
  }

  static last(): FakeSocket {
    const socket = FakeSocket.all.at(-1);
    if (!socket) throw new Error("no socket was opened");
    return socket;
  }
}
