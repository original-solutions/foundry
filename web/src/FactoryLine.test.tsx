import { fireEvent, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { FactorySettings } from "./api";
import { SettingsFold } from "./FactoryLine";
import { renderApp } from "./test-utils";

const settings: FactorySettings = { paused: false, poll_seconds: 30, feedback_seconds: 300, max_runs: 2, max_gates: 3, model: "opus", effort: "high" };
const line = "opus · high effort · 2 runs at once · 3 gates at once · looks every 30s · feedback every 300s";

describe("SettingsFold", () => {
  it("is a link to Settings → Factory that keeps its summary line, not a control that opens a form", async () => {
    await renderApp(<SettingsFold settings={settings} />);
    const link = await screen.findByRole("link", { name: /Settings for every project/ });
    expect(link.getAttribute("href")).toBe("/settings/factory");
    expect(link.textContent).toContain(line);
    expect(screen.queryByRole("button", { name: "Settings for every project" })).toBeNull();
    fireEvent.click(link);
    expect(screen.queryByRole("form", { name: "Factory settings" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Save" })).toBeNull();
  });
});
