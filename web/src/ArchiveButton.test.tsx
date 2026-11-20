import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ArchiveButton } from "./ArchiveButton";
import { json, mockFetch, renderApp } from "./test-utils";

afterEach(() => vi.unstubAllGlobals());

/** Clicks the row's Archive, then the confirming Archive in the dialog it opens. */
async function archive() {
  fireEvent.click(await screen.findByRole("button", { name: "Archive" }));
  const dialog = await screen.findByRole("dialog");
  fireEvent.click(within(dialog).getByRole("button", { name: "Archive" }));
  return dialog;
}

describe("ArchiveButton", () => {
  it("asks first, saying what archiving does, and sends nothing until confirmed", async () => {
    const calls = mockFetch();
    await renderApp(<ArchiveButton workspace="w1" name="e2e work" />);

    fireEvent.click(await screen.findByRole("button", { name: "Archive" }));

    const dialog = await screen.findByRole("dialog");
    expect(dialog.textContent).toContain("Archive e2e work?");
    expect(dialog.textContent).toMatch(/terminals stop/);
    expect(dialog.textContent).toMatch(/branch stays/);
    fireEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(calls).toHaveLength(0);
  });

  it("archives without force once confirmed, and closes on 204", async () => {
    const calls = mockFetch(new Response(null, { status: 204 }));
    await renderApp(<ArchiveButton workspace="w1" name="e2e work" />);

    await archive();

    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(calls).toEqual([{ url: "/api/workspaces/w1/archive", method: "POST", body: { force: false } }]);
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("lists what holds an archive up on 409 and archives anyway with force", async () => {
    const calls = mockFetch(
      json(409, { error: "workspace has unsaved work", uncommitted: ["src/a.rs"], unpushed: ["abc123 wip"], submodules: ["vendor/x"] }),
      new Response(null, { status: 204 }),
    );
    await renderApp(<ArchiveButton workspace="w1" name="e2e work" />);

    const dialog = await archive();

    await waitFor(() => expect(dialog.textContent).toContain("src/a.rs"));
    expect(dialog.textContent).toContain("abc123 wip");
    expect(dialog.textContent).toContain("vendor/x");

    fireEvent.click(within(dialog).getByRole("button", { name: "Archive anyway" }));

    await waitFor(() => expect(calls).toHaveLength(2));
    expect(calls[1]).toEqual({ url: "/api/workspaces/w1/archive", method: "POST", body: { force: true } });
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("shows the text of any other error in the dialog", async () => {
    mockFetch(json(500, { error: "disk on fire" }));
    await renderApp(<ArchiveButton workspace="w1" name="e2e work" />);

    const dialog = await archive();

    expect((await within(dialog).findByRole("alert")).textContent).toContain("disk on fire");
  });
});
