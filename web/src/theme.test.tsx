import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import tailwindTheme from "tailwindcss/theme.css?raw";
import appCss from "./index.css?raw";
import { ThemeButton } from "./ThemeButton";
import { SettingsAppearance } from "./SettingsAppearance";
import { applyTheme, flippedShade, setTheme, storedTheme, useTheme } from "./theme";
import { pageProps } from "./test-utils";

const sources = import.meta.glob(["./**/*.tsx", "./**/*.ts", "!./**/*.test.*"], { query: "?raw", import: "default", eager: true }) as Record<string, string>;

/** `--color-<hue>-<shade>: <value>` pairs in a block of CSS. */
function colours(css: string): Map<string, string> {
  return new Map([...css.matchAll(/--color-([a-z]+-\d+):\s*([^;]+);/g)].map((m) => [m[1], m[2].trim()]));
}

/** The body of the rule whose selector is exactly `selector`. */
function rule(css: string, selector: string): string {
  const start = css.indexOf(`${selector} {`);
  if (start < 0) return "";
  return css.slice(start, css.indexOf("}", start));
}

afterEach(() => {
  localStorage.clear();
  delete document.documentElement.dataset.theme;
  vi.restoreAllMocks();
});

describe("the light theme's palette", () => {
  const dark = colours(tailwindTheme);
  const light = colours(rule(appCss, ':root[data-theme="light"]'));

  it("mirrors each shade across the scale: 50 and 950 swap, 500 stays", () => {
    expect(flippedShade(50)).toBe(950);
    expect(flippedShade(950)).toBe(50);
    expect(flippedShade(300)).toBe(700);
    expect(flippedShade(500)).toBe(500);
  });

  it("gives every colour the UI uses its mirrored shade from Tailwind's palette", () => {
    const used = new Set<string>();
    for (const text of Object.values(sources)) {
      for (const m of text.matchAll(/\b(?:bg|text|border|ring|fill|stroke|from|to|via|outline|shadow|divide|caret|accent|decoration|placeholder)-([a-z]+)-(\d{2,3})\b/g)) {
        used.add(m[1]);
      }
    }
    expect(used.size).toBeGreaterThan(3);
    expect(dark.size).toBeGreaterThan(200);
    for (const hue of used) {
      for (const shade of [50, 100, 200, 300, 400, 500, 600, 700, 800, 900, 950]) {
        const name = `${hue}-${shade}`;
        expect(light.get(name), name).toBe(dark.get(`${hue}-${flippedShade(shade)}`));
      }
    }
  });

  it("keeps the terminal well dark: black is not remapped", () => {
    expect(light.has("black")).toBe(false);
    expect(rule(appCss, ':root[data-theme="light"]')).toContain("color-scheme: light");
  });
});

describe("the theme choice", () => {
  it("is dark until the user picks light, and comes back after a reload", () => {
    expect(storedTheme()).toBe("dark");
    render(<ThemeButton />);
    fireEvent.click(screen.getByRole("button", { name: "Switch to light theme" }));
    expect(document.documentElement.dataset.theme).toBe("light");
    expect(localStorage.getItem("foundry.theme")).toBe("light");
    expect(storedTheme()).toBe("light");

    fireEvent.click(screen.getByRole("button", { name: "Switch to dark theme" }));
    expect(document.documentElement.dataset.theme).toBe("dark");
    expect(storedTheme()).toBe("dark");
  });

  it("starts from the stored choice", () => {
    localStorage.setItem("foundry.theme", "light");
    applyTheme(storedTheme());
    render(<ThemeButton />);
    expect(document.documentElement.dataset.theme).toBe("light");
    expect(screen.getByRole("button", { name: "Switch to dark theme" })).toBeTruthy();
  });

  it("still switches when the browser blocks storage", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    expect(storedTheme()).toBe("dark");
    render(<ThemeButton />);
    fireEvent.click(screen.getByRole("button", { name: "Switch to light theme" }));
    expect(document.documentElement.dataset.theme).toBe("light");
  });
});

describe("the shared theme state", () => {
  it("shows a change made in one place in the other at once", () => {
    function Both() {
      return (
        <>
          <ThemeButton />
          <SettingsAppearance {...pageProps()} />
        </>
      );
    }
    render(<Both />);
    const dark = () => screen.getByRole("radio", { name: "Dark" });
    const light = () => screen.getByRole("radio", { name: "Light" });
    expect(dark().getAttribute("aria-checked")).toBe("true");

    fireEvent.click(screen.getByRole("button", { name: "Switch to light theme" }));
    expect(light().getAttribute("aria-checked")).toBe("true");
    expect(document.documentElement.dataset.theme).toBe("light");

    fireEvent.click(dark());
    expect(screen.getByRole("button", { name: "Switch to light theme" })).toBeDefined();
    expect(document.documentElement.dataset.theme).toBe("dark");
    expect(localStorage.getItem("foundry.theme")).toBe("dark");
  });

  it("setTheme applies, keeps and tells every user of useTheme", () => {
    function Shows() {
      return <output data-testid="theme">{useTheme()}</output>;
    }
    render(<Shows />);
    expect(screen.getByTestId("theme").textContent).toBe("dark");
    act(() => setTheme("light"));
    expect(screen.getByTestId("theme").textContent).toBe("light");
    expect(document.documentElement.dataset.theme).toBe("light");
    expect(storedTheme()).toBe("light");
  });
});
