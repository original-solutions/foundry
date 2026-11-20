import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider } from "@tanstack/react-router";
import { act, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { UiMessage } from "./api";
import { router } from "./App";
import { ME, sample, settings, terminal, workspace } from "./test-fixtures";
import { FakeSocket } from "./test-utils";

// jsdom cannot run xterm.js.
vi.mock("@xterm/xterm", () => ({
  Terminal: class {
    cols = 80;
    rows = 24;
    loadAddon() {}
    open() {}
    reset() {}
    write() {}
    onData() {
      return { dispose() {} };
    }
    onResize() {
      return { dispose() {} };
    }
    dispose() {}
  },
}));
vi.mock("@xterm/addon-fit", () => ({ FitAddon: class { fit() {} } }));

const sockets = (path: string) => FakeSocket.all.filter((s) => s.url.endsWith(path));

beforeEach(() => {
  FakeSocket.install();
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
});

afterEach(() => vi.unstubAllGlobals());

describe("App", () => {
  it("gives the pane its terminal's state from the events socket, so an exited one is not tried again", async () => {
    window.history.pushState({}, "", "/terminals/t1");
    await router.load();
    render(
      <QueryClientProvider client={new QueryClient()}>
        <RouterProvider router={router} />
      </QueryClientProvider>,
    );

    const events = sockets("/api/events")[0];
    const state: UiMessage = {
      type: "state",
      protocol: 1,
      records: [sample(ME, 1), workspace(ME, "w1", "fix login"), terminal(ME, "t1", "w1", "exited", { exit_code: 0 })],
      settings: settings(),
    };
    act(() => events.open());
    act(() => events.deliver(JSON.stringify(state)));
    const pane = sockets("/api/terminals/t1")[0];
    act(() => pane.end(1006));
    await new Promise((resolve) => setTimeout(resolve, 700));

    expect(sockets("/api/terminals/t1")).toHaveLength(1);
    expect(screen.getByText(/no longer running/i)).toBeDefined();
  });
});
