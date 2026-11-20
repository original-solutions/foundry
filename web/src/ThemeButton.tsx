import { useState, type MouseEvent } from "react";
import { flushSync } from "react-dom";
import { applyTheme, saveTheme, storedTheme, type Theme } from "./theme";

/** Flips between the dark and light themes. The new theme spreads out from the button as a circle. */
export function ThemeButton({ className = "" }: { className?: string }) {
  const [theme, setTheme] = useState<Theme>(storedTheme);
  const next: Theme = theme === "dark" ? "light" : "dark";

  const flip = (event: MouseEvent<HTMLButtonElement>) => {
    const swap = () => {
      flushSync(() => setTheme(next));
      applyTheme(next);
      saveTheme(next);
    };
    const still = typeof window.matchMedia === "function" && window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    if (!document.startViewTransition || still) {
      swap();
      return;
    }
    const box = event.currentTarget.getBoundingClientRect();
    const x = box.left + box.width / 2;
    const y = box.top + box.height / 2;
    const root = document.documentElement.style;
    root.setProperty("--reveal-x", `${x}px`);
    root.setProperty("--reveal-y", `${y}px`);
    root.setProperty("--reveal-r", `${Math.hypot(Math.max(x, innerWidth - x), Math.max(y, innerHeight - y))}px`);
    document.startViewTransition(swap);
  };

  return (
    <button
      type="button"
      aria-label={`Switch to ${next} theme`}
      title={`Switch to ${next} theme`}
      onClick={flip}
      className={`rounded p-1 text-neutral-400 hover:bg-neutral-800 hover:text-neutral-200 ${className}`}
    >
      {theme === "dark" ? (
        <svg viewBox="0 0 16 16" className="size-4" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
          <circle cx="8" cy="8" r="3" />
          <path d="M8 1.5v1.5M8 13v1.5M1.5 8H3M13 8h1.5M3.4 3.4l1.06 1.06M11.54 11.54l1.06 1.06M3.4 12.6l1.06-1.06M11.54 4.46l1.06-1.06" strokeLinecap="round" />
        </svg>
      ) : (
        <svg viewBox="0 0 16 16" className="size-4" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
          <path d="M13.5 9.6A5.75 5.75 0 0 1 6.4 2.5a5.75 5.75 0 1 0 7.1 7.1Z" strokeLinejoin="round" />
        </svg>
      )}
    </button>
  );
}
