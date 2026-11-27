import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { faces, Faces, faceOf } from "./face";
import { ProjectFace } from "./ProjectFace";

describe("ProjectFace", () => {
  it("draws the project's letters on the hue the fleet's list gives it", () => {
    const listed = faces([
      { id: "a1", name: "storefront", repo: "" },
      { id: "b2", name: "billing-api", repo: "" },
    ]);
    render(
      <Faces.Provider value={listed}>
        <ProjectFace project="billing-api" />
      </Faces.Provider>,
    );
    const face = screen.getByTitle("billing-api");
    expect(face.textContent).toBe("BA");
    expect(face.style.getPropertyValue("--hue")).toBe(String(listed.get("billing-api")?.hue));
  });

  it("draws a project the list does not hold from its name alone", () => {
    render(<ProjectFace project="gone" title="old work · gone" />);
    const face = screen.getByTitle("old work · gone");
    expect(face.textContent).toBe("GO");
    expect(face.style.getPropertyValue("--hue")).toBe(String(faceOf("gone").hue));
  });
});
