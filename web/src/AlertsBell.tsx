import { useEffect, useMemo, useRef, useState } from "react";
import { alertsFor, favicon, loadAlertsOn, pulse, rippleFrames, saveAlertsOn, tabTitle, type Seen } from "./alerts";
import type { MachineView } from "./fleet";

export type AlertsStatus = "unsupported" | "blocked" | "off" | "on";

const supported = () => typeof Notification === "function";

/**
 * The fleet reaching past the page. The tab's title counts what needs you and what works, and its
 * icon is the fleet's most urgent glyph (rippling while an agent works, still under reduced
 * motion). With alerts turned on, a session that finishes its turn or needs you raises a desktop
 * notification while the page does not have focus; a click on it opens that session's terminal.
 */
export function useAlerts(machines: MachineView[], synced: boolean, onOpen: (terminal: string) => void) {
  const [on, setOn] = useState(loadAlertsOn);
  const [permission, setPermission] = useState(() => (supported() ? Notification.permission : undefined));
  const status: AlertsStatus = !supported() ? "unsupported" : permission === "denied" ? "blocked" : permission === "granted" && on ? "on" : "off";

  const open = useRef(onOpen);
  useEffect(() => {
    open.current = onOpen;
  });

  const seen = useRef<Seen>(undefined);
  useEffect(() => {
    if (!synced) return;
    const next = alertsFor(seen.current, machines);
    seen.current = next.seen;
    if (status !== "on" || document.hasFocus()) return;
    for (const alert of next.alerts) {
      try {
        const shown = new Notification(alert.title, { body: alert.body, tag: alert.tag, icon: favicon(alert.glyph) });
        shown.onclick = () => {
          window.focus();
          open.current(alert.terminal);
          shown.close();
        };
      } catch {
        // A browser that only shows notifications from a service worker (Chrome on Android).
      }
    }
  }, [machines, synced, status]);

  const now = useMemo(() => pulse(machines), [machines]);
  const title = tabTitle(now);
  useEffect(() => {
    document.title = title;
  }, [title]);

  useEffect(() => {
    let link = document.head.querySelector<HTMLLinkElement>("link[rel=icon]");
    if (!link) {
      link = document.createElement("link");
      link.rel = "icon";
      document.head.append(link);
    }
    const icon = link;
    icon.href = favicon(now.glyph);
    const still = window.matchMedia?.("(prefers-reduced-motion: reduce)").matches ?? false;
    if (now.glyph !== "working" || still) return;
    let frame = 0;
    const timer = setInterval(() => {
      frame = (frame + 1) % rippleFrames.length;
      icon.href = favicon("working", rippleFrames[frame]);
    }, 400);
    return () => clearInterval(timer);
  }, [now.glyph]);

  const toggle = async () => {
    if (status === "on") {
      setOn(false);
      saveAlertsOn(false);
      return;
    }
    if (status !== "off") return;
    const answer = permission === "granted" ? permission : await Notification.requestPermission();
    setPermission(answer);
    if (answer !== "granted") return;
    setOn(true);
    saveAlertsOn(true);
  };

  return { status, onToggle: () => void toggle() };
}

const hint: Record<Exclude<AlertsStatus, "unsupported">, string> = {
  off: "Alert me when an agent finishes its turn or needs you, while Foundry is in the background",
  on: "Alerts on: a desktop alert when an agent finishes its turn or needs you. Click to turn off",
  blocked: "Alerts are blocked for this site. Allow notifications in the browser's site settings",
};

/** The bell that turns desktop alerts on and off; nothing where the browser has no notifications. */
export function AlertsButton({ status, onToggle }: { status: AlertsStatus; onToggle: () => void }) {
  if (status === "unsupported") return null;
  const tone = status === "on" ? "text-sky-400 hover:text-sky-300" : status === "blocked" ? "text-neutral-600" : "text-neutral-400 hover:text-neutral-200";
  return (
    <button
      type="button"
      aria-label={`Alerts ${status}`}
      aria-pressed={status === "blocked" ? undefined : status === "on"}
      aria-disabled={status === "blocked" || undefined}
      title={hint[status]}
      onClick={onToggle}
      className={`rounded p-1 hover:bg-neutral-800 ${tone}`}
    >
      <svg viewBox="0 0 16 16" className="size-4" fill={status === "on" ? "currentColor" : "none"} stroke="currentColor" strokeWidth="1.4" aria-hidden="true">
        <path d="M8 2.2a3.8 3.8 0 0 0-3.8 3.8v2.6L3 10.8h10l-1.2-2.2V6A3.8 3.8 0 0 0 8 2.2Z" strokeLinejoin="round" />
        <path d="M6.6 12.8a1.5 1.5 0 0 0 2.8 0" strokeLinecap="round" fill="none" />
        {status === "blocked" && <path d="M2.5 2.5l11 11" strokeLinecap="round" />}
      </svg>
    </button>
  );
}
