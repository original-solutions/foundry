import { useLayoutEffect, useState, type RefObject } from "react";

/** Tailwind's `md` breakpoint, as a media query: the screen is wide from here up. */
export const MD = "(min-width: 768px)";

/** Whether `query` matches now; `fallback` in a browser with no `matchMedia`. */
export function matches(query: string, fallback: boolean): boolean {
  return typeof matchMedia === "function" ? matchMedia(query).matches : fallback;
}

/** Whether `query` matches, following it as it changes; `fallback` in a browser with no `matchMedia`. */
export function useMediaQuery(query: string, fallback: boolean): boolean {
  const [now, setNow] = useState(() => matches(query, fallback));
  useLayoutEffect(() => {
    if (typeof matchMedia !== "function") return;
    const media = matchMedia(query);
    const change = () => setNow(media.matches);
    change();
    media.addEventListener?.("change", change);
    return () => media.removeEventListener?.("change", change);
  }, [query]);
  return now;
}

/**
 * How wide `ref`'s element is, following it as it changes; unknown until it is laid out, and in a
 * browser that cannot say.
 */
export function useWidth(ref: RefObject<HTMLElement | null>): number | undefined {
  const [width, setWidth] = useState<number>();
  useLayoutEffect(() => {
    const element = ref.current;
    if (!element) return;
    // A hidden element measures 0 wide: not known, rather than as narrow as can be.
    const measure = () => setWidth(element.getBoundingClientRect().width || undefined);
    measure();
    if (typeof ResizeObserver !== "function") return;
    const observer = new ResizeObserver(measure);
    observer.observe(element);
    return () => observer.disconnect();
  }, [ref]);
  return width;
}
