import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, renderHook, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Briefs, SessionState } from "./api";
import { RETRY_EVERY } from "./poll";
import { ME, session } from "./test-fixtures";
import { json } from "./test-utils";
import { BRIEFS_EVERY, useBriefs } from "./useBriefs";

const working: Briefs = { s1: { title: "fix the login bug", now: { kind: "tool", tool: "Edit", text: "Edit src/login.ts" } } };

let answer: () => Response;
let reads = 0;

beforeEach(() => {
  reads = 0;
  answer = () => json(200, working);
  vi.useFakeTimers({ shouldAdvanceTime: true });
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => {
      reads += 1;
      return answer();
    }),
  );
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

const fleetIn = (state: SessionState) => [session(ME, "s1", "w1", "t1", state)];

async function watch(state: SessionState) {
  const client = new QueryClient();
  const wrapper = ({ children }: { children: ReactNode }) => <QueryClientProvider client={client}>{children}</QueryClientProvider>;
  const view = renderHook(({ records }) => useBriefs(records), { initialProps: { records: fleetIn(state) }, wrapper });
  await waitFor(() => expect(reads).toBe(1));
  await act(async () => {});
  return { client, ...view };
}

const wait = (ms: number) => act(() => vi.advanceTimersByTimeAsync(ms));

describe("useBriefs", () => {
  it("reads every BRIEFS_EVERY while a session works or starts", async () => {
    for (const state of ["working", "starting"] as const) {
      reads = 0;
      const { unmount } = await watch(state);
      await wait(5 * BRIEFS_EVERY + 100);
      expect(reads).toBe(6);
      unmount();
    }
  });

  it("does not read on a timer while a session waits for you or is stuck: only a hook changes those", async () => {
    for (const state of ["waiting_for_you", "stuck"] as const) {
      reads = 0;
      const { unmount } = await watch(state);
      await wait(60_000);
      expect(reads).toBe(1);
      unmount();
    }
  });

  it("reads again when a session changes state, refreshing the one entry rather than adding one", async () => {
    const { client, rerender } = await watch("waiting_for_you");
    rerender({ records: fleetIn("idle") });
    await waitFor(() => expect(reads).toBe(2));
    rerender({ records: fleetIn("finished") });
    await waitFor(() => expect(reads).toBe(3));
    expect(client.getQueryCache().findAll({ queryKey: ["briefs"] })).toHaveLength(1);
  });

  it("reads once when the first sessions arrive, as when the fleet's state comes in", async () => {
    const client = new QueryClient();
    const wrapper = ({ children }: { children: ReactNode }) => <QueryClientProvider client={client}>{children}</QueryClientProvider>;
    const { rerender } = renderHook(({ records }) => useBriefs(records), { initialProps: { records: [] as ReturnType<typeof fleetIn> }, wrapper });
    await wait(1000);
    expect(reads).toBe(0);
    rerender({ records: fleetIn("idle") });
    await wait(1000);
    expect(reads).toBe(1);
  });

  it("keeps reading after a failed read, at the back-off rather than not at all", async () => {
    answer = () => json(500, { error: "daemon hiccup" });
    await watch("working");
    await wait(BRIEFS_EVERY + 100);
    expect(reads).toBe(1);
    answer = () => json(200, working);
    await wait(RETRY_EVERY - BRIEFS_EVERY);
    expect(reads).toBe(2);
    await wait(BRIEFS_EVERY);
    expect(reads).toBe(3);
  });

  it("stops reading on a 404", async () => {
    answer = () => json(404, { error: "not found" });
    await watch("working");
    await wait(60_000);
    expect(reads).toBe(1);
  });

  it("shows no current step once a read fails, rather than the stale one, and keeps the title", async () => {
    const { result } = await watch("working");
    expect(result.current.s1.now?.text).toBe("Edit src/login.ts");
    answer = () => json(500, { error: "daemon hiccup" });
    await wait(BRIEFS_EVERY);
    await waitFor(() => expect(reads).toBe(2));
    await waitFor(() => expect(result.current.s1.now).toBeNull());
    expect(result.current.s1.title).toBe("fix the login bug");
  });
});
