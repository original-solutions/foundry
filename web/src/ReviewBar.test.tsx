import { act, fireEvent, screen } from "@testing-library/react";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import type { Change } from "./reviewDiff";
import { ReviewBar } from "./ReviewBar";
import { renderApp } from "./test-utils";

const runs: Change = { key: "max_runs", label: "Runs at once", before: "3", after: "4" };
const paused: Change = { key: "paused", label: "Pause claims", before: "off", after: "on" };

const bar = (props: Partial<Parameters<typeof ReviewBar>[0]> = {}) =>
  renderApp(<ReviewBar changes={[runs, paused]} onSave={async () => []} onDiscard={() => {}} stale={[]} {...props} />);

describe("ReviewBar", () => {
  it("is hidden with no changes", async () => {
    await bar({ changes: [] });
    expect(screen.queryByRole("button", { name: /Save/ })).toBeNull();
    expect(screen.queryByRole("button", { name: "Discard" })).toBeNull();
  });

  it("lists each change as old to new, with the count on Save", async () => {
    await bar();
    const region = await screen.findByRole("region", { name: "Unsaved changes" });
    expect(region.textContent).toContain("2 changes");
    expect(region.textContent).toContain("Runs at once 3 → 4");
    expect(region.textContent).toContain("Pause claims off → on");
    expect(screen.getByRole("button", { name: "Save 2 changes" })).toBeDefined();
  });

  it("says one change in the singular", async () => {
    await bar({ changes: [runs] });
    expect((await screen.findByRole("region", { name: "Unsaved changes" })).textContent).toContain("1 change ·");
    expect(screen.getByRole("button", { name: "Save 1 change" })).toBeDefined();
  });

  it("calls onDiscard from Discard", async () => {
    const onDiscard = vi.fn();
    await bar({ onDiscard });
    fireEvent.click(await screen.findByRole("button", { name: "Discard" }));
    expect(onDiscard).toHaveBeenCalledTimes(1);
  });

  it("shows a refusal that names a key beside that change, and one with no key beside Save", async () => {
    const onSave = vi.fn(async () => [{ key: "max_runs", message: "Too many" }, { message: "The daemon said no" }]);
    await bar({ onSave });
    fireEvent.click(await screen.findByRole("button", { name: "Save 2 changes" }));
    const row = await screen.findByRole("listitem", { name: "Runs at once" });
    expect(row.textContent).toContain("Too many");
    const other = screen.getByRole("listitem", { name: "Pause claims" });
    expect(other.textContent).not.toContain("Too many");
    const note = screen.getByText("The daemon said no");
    expect(note.closest("[data-beside]")?.getAttribute("data-beside")).toBe("save");
    expect(onSave).toHaveBeenCalledTimes(1);
  });

  it("marks a stale row, and asks for a fresh look before it calls onSave", async () => {
    const onSave = vi.fn(async () => []);
    await bar({ onSave, stale: ["max_runs"] });
    const row = await screen.findByRole("listitem", { name: "Runs at once" });
    expect(row.textContent).toContain("changed elsewhere");
    expect(screen.getByRole("listitem", { name: "Pause claims" }).textContent).not.toContain("changed elsewhere");

    fireEvent.click(screen.getByRole("button", { name: "Look again" }));
    expect(onSave).not.toHaveBeenCalled();
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Save 2 changes" })));
    expect(onSave).toHaveBeenCalledTimes(1);
  });

  it("shows a refusal keyed to a field with no change beside Save", async () => {
    const onSave = vi.fn(async () => [{ key: "model", message: "no such model" }]);
    await bar({ onSave, changes: [runs] });
    fireEvent.click(await screen.findByRole("button", { name: "Save 1 change" }));
    const note = await screen.findByText("no such model");
    expect(note.closest("[data-beside]")?.getAttribute("data-beside")).toBe("save");
  });

  it("tells onLook when Look again is pressed, and sends nothing", async () => {
    const onLook = vi.fn();
    const onSave = vi.fn(async () => []);
    await bar({ onSave, onLook, stale: ["max_runs"] });
    fireEvent.click(await screen.findByRole("button", { name: "Look again" }));
    expect(onLook).toHaveBeenCalledTimes(1);
    expect(onSave).not.toHaveBeenCalled();
  });

  it("asks for a fresh look again when a field goes stale again, or a different one does", async () => {
    let set: (stale: string[]) => void = () => {};
    function Harness() {
      const [stale, setStale] = useState<string[]>(["max_runs"]);
      set = setStale;
      return <ReviewBar changes={[runs, paused]} onSave={async () => []} onDiscard={() => {}} stale={stale} />;
    }
    await renderApp(<Harness />);
    fireEvent.click(await screen.findByRole("button", { name: "Look again" }));
    expect(screen.getByRole("button", { name: "Save 2 changes" })).toBeDefined();

    // Looked at, then no longer stale, then stale again: the look does not carry over.
    act(() => set([]));
    expect(screen.getByRole("button", { name: "Save 2 changes" })).toBeDefined();
    act(() => set(["max_runs"]));
    expect(screen.getByRole("button", { name: "Look again" })).toBeDefined();
    fireEvent.click(screen.getByRole("button", { name: "Look again" }));

    // A different field going stale on top of the looked-at one asks again.
    act(() => set(["max_runs", "paused"]));
    expect(screen.getByRole("button", { name: "Look again" })).toBeDefined();
  });
});
