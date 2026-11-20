import { fireEvent, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ArchiveButton } from "./ArchiveButton";
import { renderApp } from "./test-utils";

type Call = { url: string; method: string; body: unknown };

function mockFetch(...answers: Response[]) {
  const calls: Call[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init: RequestInit = {}) => {
      calls.push({ url, method: init.method ?? "GET", body: init.body ? JSON.parse(String(init.body)) : undefined });
      const next = answers.shift();
      if (!next) throw new Error("unexpected request");
      return next;
    }),
  );
  return calls;
}

const json = (status: number, body: unknown) => new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

afterEach(() => vi.unstubAllGlobals());

describe("ArchiveButton", () => {
  it("archives without force and stays quiet on 204", async () => {
    const calls = mockFetch(new Response(null, { status: 204 }));
    await renderApp(<ArchiveButton workspace="w1" name="e2e work" />);

    fireEvent.click(await screen.findByRole("button", { name: "Archive" }));

    await waitFor(() => expect(calls).toHaveLength(1));
    expect(calls[0]).toEqual({ url: "/api/workspaces/w1/archive", method: "POST", body: { force: false } });
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("lists what holds an archive up on 409 and archives anyway with force", async () => {
    const calls = mockFetch(
      json(409, { error: "workspace has unsaved work", uncommitted: ["src/a.rs"], unpushed: ["abc123 wip"], submodules: ["vendor/x"] }),
      new Response(null, { status: 204 }),
    );
    await renderApp(<ArchiveButton workspace="w1" name="e2e work" />);

    fireEvent.click(await screen.findByRole("button", { name: "Archive" }));

    const dialog = await screen.findByRole("dialog");
    expect(dialog.textContent).toContain("src/a.rs");
    expect(dialog.textContent).toContain("abc123 wip");
    expect(dialog.textContent).toContain("vendor/x");

    fireEvent.click(screen.getByRole("button", { name: "Archive anyway" }));

    await waitFor(() => expect(calls).toHaveLength(2));
    expect(calls[1]).toEqual({ url: "/api/workspaces/w1/archive", method: "POST", body: { force: true } });
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("shows the text of any other error", async () => {
    mockFetch(json(500, { error: "disk on fire" }));
    await renderApp(<ArchiveButton workspace="w1" name="e2e work" />);

    fireEvent.click(await screen.findByRole("button", { name: "Archive" }));

    expect((await screen.findByRole("alert")).textContent).toContain("disk on fire");
  });
});
