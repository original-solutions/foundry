import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { Modal } from "./Modal";

describe("Modal", () => {
  for (const wide of [false, true]) {
    it(`${wide ? "wide" : "plain"}: shows above the drawer and other dialogs, and scrolls when taller than the window`, () => {
      render(
        <Modal open onOpenChange={() => {}} title="T" description="D" wide={wide}>
          body
        </Modal>,
      );

      const dialog = screen.getByRole("dialog", { name: "T" });
      expect(dialog.classList).toContain("z-50");
      expect(dialog.classList).toContain("max-h-[calc(100dvh-1.5rem)]");
      expect(dialog.classList).toContain("overflow-y-auto");

      const overlay = dialog.previousElementSibling;
      expect(overlay?.classList).toContain("fixed");
      expect(overlay?.classList).toContain("inset-0");
      expect(overlay?.classList).toContain("z-50");
    });
  }
});
