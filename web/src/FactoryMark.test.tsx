import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { FactoryMark } from "./FactoryMark";

const run = { run: "original-solutions__foundry__162__1", issue: 162, url: "https://github.com/original-solutions/foundry/issues/162" };

describe("FactoryMark", () => {
  it("says factory and the issue in full by default, as the board and its list show it", () => {
    render(<FactoryMark run={run} />);
    const mark = screen.getByRole("link", { name: "factory #162" });
    expect(mark.textContent).toBe("factory #162");
    expect(mark.hasAttribute("aria-label")).toBe(false);
    expect(mark.getAttribute("title")).toBe("Built by the factory: open its issue");
  });

  it("shows only the issue when compact, still named in full", () => {
    render(<FactoryMark run={run} compact />);
    const mark = screen.getByRole("link", { name: "factory #162" });
    expect(mark.textContent).toBe("#162");
    expect(mark.getAttribute("aria-label")).toBe("factory #162");
    expect(mark.getAttribute("title")).toBe("Built by the factory: open its issue");
  });

  it("says factory, compact or not, for a run that names no issue", () => {
    const { rerender } = render(<FactoryMark run={{ run: "not-a-run" }} compact />);
    const mark = () => screen.getByText("factory", { exact: true });
    expect(mark().tagName).toBe("SPAN");
    expect(mark().hasAttribute("aria-label")).toBe(false);
    rerender(<FactoryMark run={{ run: "not-a-run" }} />);
    expect(mark().hasAttribute("aria-label")).toBe(false);
  });
});
