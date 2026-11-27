import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { useLeaving } from "./Archive";
import { changesQuery } from "./ChangesPanel";
import type { Card } from "./lanes";
import { BOARD_BUSY_EVERY, pollEvery } from "./poll";
import { shippingQuery } from "./Shipping";

/** Whether the element is on screen, or near it; always true where the browser cannot tell. */
export function useOnScreen<T extends Element>() {
  const ref = useRef<T>(null);
  const [onScreen, setOnScreen] = useState(() => typeof IntersectionObserver === "undefined");
  useLayoutEffect(() => {
    const element = ref.current;
    if (!element || typeof IntersectionObserver === "undefined") return;
    const observer = new IntersectionObserver((entries) => setOnScreen(entries[entries.length - 1].isIntersecting), { rootMargin: "200px" });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);
  return [ref, onScreen] as const;
}

/**
 * What a board card's workspace changed since its base, read as the sheet reads it, but only while
 * the card is on screen: when it comes on screen, whenever one of its terminals changes state (so
 * the end of a turn shows what the turn wrote), and every `BOARD_BUSY_EVERY` while an agent in it
 * works. A state change off screen is read when the card comes back. A failed read is tried again
 * on the timer; a workspace the daemon does not have (a 404) is not asked again on a timer. Once
 * its workspace is being archived, nothing is read. A card and a line of the list read the same way.
 */
export function useCardChanges(card: Card, onScreen: boolean) {
  const busy = card.rows.some((r) => r.glyph === "working");
  const workspace = card.workspace.id;
  const client = useQueryClient();
  const leaving = useLeaving(workspace);
  const { data, error } = useQuery({
    ...changesQuery(workspace),
    enabled: onScreen && !leaving,
    staleTime: BOARD_BUSY_EVERY,
    retry: false,
    refetchInterval: pollEvery(BOARD_BUSY_EVERY, busy),
  });
  const states = card.rows.map((r) => `${r.terminal}:${r.glyph}`).join(" ");
  const last = useRef(states);
  useEffect(() => {
    if (last.current === states) return;
    last.current = states;
    if (leaving) return;
    // Read now if on screen; otherwise marked out of date, and read when it comes on screen.
    void client.invalidateQueries({ queryKey: changesQuery(workspace).queryKey, exact: true });
    // A turn that ends may have pushed or opened a pull request.
    void client.invalidateQueries({ queryKey: shippingQuery(workspace).queryKey, exact: true });
  }, [states, client, workspace, leaving]);
  return { data, error, leaving };
}
