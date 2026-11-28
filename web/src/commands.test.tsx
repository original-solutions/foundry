import { screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { useCommands } from "./commands";
import type { Needing } from "./needing";
import { renderApp } from "./test-utils";

function Abouts({ next }: { next: Needing }) {
  const commands = useCommands({ screen: "home", next: { next, others: 0 }, onNext: vi.fn(), onCreate: vi.fn(), onKeys: vi.fn() });
  return <p>{commands.find((c) => c.key === "next")?.about}</p>;
}

describe("useCommands", () => {
  it("says a held-up factory run's whole reason in the next-that-needs-you command, not its cut label", async () => {
    const reason = "gate red: make check fails on main, see the gate log";
    const next: Needing = { terminal: "t1", workspace: "fac/1027-2", name: "opus", glyph: "done", label: "Stuck · gate red: make check…", fullLabel: `Stuck · ${reason}` };
    await renderApp(<Abouts next={next} />);
    expect(await screen.findByText(`fac/1027-2 / opus: Stuck · ${reason}.`)).toBeDefined();
  });
});
