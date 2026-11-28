import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { FactoryMark } from "./FactoryMark";

describe("FactoryMark", () => {
  it("names a run's stage in its tooltip when the run names no issue to link", () => {
    render(<FactoryMark run={{ run: "x", step: "building" }} />);
    expect(screen.getByText("factory").getAttribute("title")).toBe("Factory run, Building");
  });

  it("has no tooltip when the run's stage is not known", () => {
    render(<FactoryMark run={{ run: "x" }} />);
    expect(screen.getByText("factory").getAttribute("title")).toBeNull();
  });
});
