import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, RouterProvider } from "@tanstack/react-router";
import { render } from "@testing-library/react";
import type { ReactNode } from "react";
import { vi } from "vitest";

/** Renders `ui` inside a query client and a router that knows the terminal route, so links resolve. Gives back the query client too. */
export async function renderApp(ui: ReactNode) {
  const root = createRootRoute({ component: () => <>{ui}</> });
  const terminal = createRoute({ getParentRoute: () => root, path: "/terminals/$terminalId" });
  const router = createRouter({
    routeTree: root.addChildren([terminal]),
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
 * Stubs `fetch` to give `answers` in order, and returns the requests it was sent. The layout's reads
 * of session briefs and of the attention list get none and are left out, so a test sees only the
 * requests it makes.
 */
export function mockFetch(...answers: Response[]): Call[] {
  const calls: Call[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init: RequestInit = {}) => {
      if (url === "/api/briefs") return json(200, {});
      if (url === "/api/attention") return json(200, { items: [] });
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
