import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { Glyph } from "./labels";
import { glyphFill, glyphText, shapes, StateGlyph } from "./StateGlyph";

const glyphs: Glyph[] = ["starting", "working", "waiting", "ready", "done", "failed", "stuck", "away"];

function dots(glyph: Glyph) {
  const { container } = render(<StateGlyph glyph={glyph} />);
  return [...container.querySelectorAll("circle")];
}

describe("StateGlyph", () => {
  it("gives every glyph its own shape, so states read without colour", () => {
    expect(new Set(glyphs.map((g) => shapes[g])).size).toBe(glyphs.length);
    for (const g of glyphs) expect(shapes[g]).toMatch(/^[#.]{9}$/);
  });

  it("draws a 3 by 3 grid with the shape's dots lit", () => {
    for (const g of glyphs) {
      const all = dots(g);
      expect(all).toHaveLength(9);
      expect(all.map((d) => (d.dataset.lit ? "#" : ".")).join("")).toBe(shapes[g]);
    }
  });

  it("animates only a working glyph, as a ripple from the centre out", () => {
    for (const g of glyphs.filter((g) => g !== "working")) {
      expect(dots(g).some((d) => d.getAttribute("class")?.includes("animate-ripple")), g).toBe(false);
    }
    const working = dots("working");
    expect(working.every((d) => d.getAttribute("class")?.includes("motion-safe:animate-ripple"))).toBe(true);
    const delay = (i: number) => parseInt(working[i].style.animationDelay, 10);
    expect(delay(4)).toBe(0);
    expect([1, 3, 5, 7].map(delay).every((d) => d > delay(4))).toBe(true);
    expect([0, 2, 6, 8].map(delay).every((d) => d > delay(1))).toBe(true);
  });

  it("holds a still working glyph to its still frame, for a heading that names the state", () => {
    const { container } = render(<StateGlyph glyph="working" still />);
    const all = [...container.querySelectorAll("circle")];
    expect(all.some((d) => d.getAttribute("class")?.includes("animate-ripple"))).toBe(false);
    expect(all.map((d) => (d.dataset.lit ? "#" : ".")).join("")).toBe(shapes.working);
  });

  it("colours only a state that needs you or is moving; a resting one is grey, and green is left to actions", () => {
    const loud: Glyph[] = ["working", "waiting", "failed", "stuck"];
    for (const g of glyphs) {
      for (const tone of [glyphText[g], glyphFill[g]]) {
        expect(tone, g).not.toMatch(/emerald|green/);
        expect(/neutral/.test(tone), `${g}: ${tone}`).toBe(!loud.includes(g));
      }
    }
    const { container } = render(<StateGlyph glyph="ready" />);
    expect(container.querySelector("svg")?.getAttribute("class")).toContain("text-neutral-");
  });
});
