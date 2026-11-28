import { screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { AnyRecord, MachineSample } from "./api";
import { fleet } from "./fleet";
import { ME, OTHER, sample, settings } from "./test-fixtures";
import { renderApp } from "./test-utils";
import { UpdateBadge } from "./UpdateBadge";

const THIRD = "machine-3";
const GONE = "machine-4";

const three = settings({
  machines: [
    { id: ME, name: "studio", address: "" },
    { id: OTHER, name: "laptop", address: "" },
    { id: THIRD, name: "mini", address: "" },
  ],
});

/** A sample of `owner` with the update fields given. */
function at(owner: string, version: number, update: Partial<Pick<MachineSample, "version" | "update_ready" | "update_refused">>): AnyRecord {
  const r = sample(owner, version) as AnyRecord & { kind: "machine_sample" };
  return { ...r, body: { ...r.body, ...update } };
}

const show = (records: AnyRecord[]) => renderApp(<UpdateBadge machines={fleet(records, three)} />);

const badge = () => screen.getByRole("link", { name: /waiting/i });

describe("UpdateBadge", () => {
  it("shows nothing while no machine has a release waiting", async () => {
    await show([at(ME, 1, { version: "0.9.0" }), at(OTHER, 1, { version: "0.9.0" })]);
    expect(screen.queryByRole("link", { name: /waiting/i })).toBeNull();
  });

  it("names each waiting tag and how many machines wait on it", async () => {
    // The lower tag's machine comes first, so the order is the badge's own: newest first.
    await show([at(ME, 1, { version: "0.8.0", update_ready: "v0.9.0" }), at(OTHER, 1, { version: "0.9.0", update_ready: "v0.9.1" }), at(THIRD, 1, { version: "0.9.0", update_ready: "v0.9.1" })]);
    expect(badge().textContent).toContain("v0.9.1 on 2 machines, v0.9.0 on 1");
  });

  it("leaves out a removed machine", async () => {
    await show([at(ME, 1, { version: "0.9.0", update_ready: "v0.9.1" }), at(GONE, 1, { version: "0.9.0", update_ready: "v0.9.1" })]);
    expect(badge().textContent).toContain("v0.9.1 on 1 machine");
    expect(badge().textContent).not.toContain("2 machines");
  });

  it("links to Settings → Machines, where the checklist is, and opens no dialog of its own", async () => {
    await show([at(ME, 1, { version: "0.9.0", update_ready: "v0.9.1" })]);
    expect(badge().getAttribute("href")).toBe("/settings/machines");
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.queryByRole("checkbox")).toBeNull();
  });
});
