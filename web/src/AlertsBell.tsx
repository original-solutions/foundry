import { Popover } from "radix-ui";
import { useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { alertsFor, favicon, pulse, rippleFrames, tabTitle, type Alert, type Seen } from "./alerts";
import type { MachineView } from "./fleet";
import { loadNotify, route, saveNotify, type Notify } from "./notify";

/** Whether this browser can raise desktop alerts for the page. */
export type DesktopStatus = "unsupported" | "blocked" | "ask" | "allowed";

const supported = () => typeof Notification === "function";

/** A batch of news for cards in the page; `n` tells one batch from the next. */
export interface News {
  n: number;
  alerts: Alert[];
}

/**
 * Every way the fleet's news reaches you, from one look at what changed. The tab's title counts
 * what needs you and what works, and its icon is the fleet's most urgent glyph (rippling while an
 * agent works, still under reduced motion). A session that finishes its turn or needs you is told
 * once: on the desktop while the page is not in front (when turned on and allowed), else as a card
 * in the page (`news`) while it shows; a click on a desktop alert opens that session's terminal,
 * and coming back to the page takes the alerts it raised off the desktop. Whatever is set, the
 * same news is `spoken`, for `Announcer` to tell a screen reader.
 */
export function useAlerts(machines: MachineView[], synced: boolean, onOpen: (terminal: string) => void) {
  const [notify, setNotify] = useState(loadNotify);
  const [permission, setPermission] = useState(() => (supported() ? Notification.permission : undefined));
  const desktop: DesktopStatus = !supported() ? "unsupported" : permission === "denied" ? "blocked" : permission === "granted" ? "allowed" : "ask";

  const open = useRef(onOpen);
  useEffect(() => {
    open.current = onOpen;
  });

  const seen = useRef<Seen>(undefined);
  const [spoken, setSpoken] = useState("");
  const [news, setNews] = useState<News>();
  const raised = useRef<Notification[]>([]);
  useEffect(() => {
    if (!synced) return;
    const next = alertsFor(seen.current, machines);
    seen.current = next.seen;
    if (next.alerts.length === 0) return;
    const said = next.alerts.map((a) => `${a.title}: ${a.body}`).join(". ");
    // The same news again is said again: a live region speaks only when its text changes.
    setSpoken((last) => (last.trim() === said && !last.endsWith("\u00a0") ? `${said}\u00a0` : said));
    const to = route({ visible: document.visibilityState === "visible", focused: document.hasFocus() }, notify, desktop === "allowed");
    if (to === "card") setNews((last) => ({ n: (last?.n ?? 0) + 1, alerts: next.alerts }));
    if (to !== "desktop") return;
    for (const alert of next.alerts) {
      try {
        const shown = new Notification(alert.title, { body: alert.body, tag: alert.tag, icon: favicon(alert.glyph) });
        raised.current.push(shown);
        shown.onclick = () => {
          window.focus();
          open.current(alert.terminal);
          shown.close();
        };
      } catch {
        // A browser that only shows notifications from a service worker (Chrome on Android).
      }
    }
  }, [machines, synced, notify, desktop]);

  // Back at the page, its alerts on the desktop are old news: the page and the catch-up have it.
  useLayoutEffect(() => {
    const back = () => {
      for (const shown of raised.current.splice(0)) shown.close();
    };
    window.addEventListener("focus", back);
    return () => window.removeEventListener("focus", back);
  }, []);

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

  const choose = async (change: Partial<Notify>) => {
    if (change.desktop && desktop !== "allowed") {
      if (desktop !== "ask") return;
      const answer = await Notification.requestPermission();
      setPermission(answer);
      if (answer !== "granted") return;
    }
    setNotify((was) => {
      const now = { ...was, ...change };
      saveNotify(now);
      return now;
    });
  };

  return { notify, desktop, onChoose: (change: Partial<Notify>) => void choose(change), spoken, news };
}

/**
 * Says `spoken` to a screen reader, politely, without showing it. A bare live region, not a
 * status role, so the page's visible status lines stay the only ones.
 */
export function Announcer({ spoken }: { spoken: string }) {
  return (
    <p aria-live="polite" aria-atomic="true" data-announcer="" className="sr-only">
      {spoken}
    </p>
  );
}

const blockedHint: Partial<Record<DesktopStatus, string>> = {
  blocked: "Blocked for this site: allow notifications in the browser's site settings.",
  unsupported: "This browser has no desktop alerts for a page.",
};

/**
 * The bell: one place to choose how news of a session reaches you. Each event comes once, as a
 * card while you are at the page or a desktop alert while you are not; the catch-up sums up what
 * happened after a while away. Filled when desktop alerts are on, struck through when nothing is.
 */
export function NotifyButton({ notify, desktop, onChoose }: { notify: Notify; desktop: DesktopStatus; onChoose: (change: Partial<Notify>) => void }) {
  const desktopOn = notify.desktop && desktop === "allowed";
  const silent = !notify.cards && !desktopOn && !notify.away;
  return (
    <Popover.Root>
      <Popover.Trigger
        aria-label="Notifications"
        title="Notifications: how a session that finishes or needs you reaches you"
        className={`rounded p-1 hover:bg-neutral-800 data-[state=open]:bg-neutral-800 ${desktopOn ? "text-sky-400 hover:text-sky-300" : "text-neutral-400 hover:text-neutral-200"}`}
      >
        <svg viewBox="0 0 16 16" className="size-4" fill={desktopOn ? "currentColor" : "none"} stroke="currentColor" strokeWidth="1.4" aria-hidden="true">
          <path d="M8 2.2a3.8 3.8 0 0 0-3.8 3.8v2.6L3 10.8h10l-1.2-2.2V6A3.8 3.8 0 0 0 8 2.2Z" strokeLinejoin="round" />
          <path d="M6.6 12.8a1.5 1.5 0 0 0 2.8 0" strokeLinecap="round" fill="none" />
          {silent && <path d="M2.5 2.5l11 11" strokeLinecap="round" />}
        </svg>
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Content
          aria-label="Notifications"
          align="end"
          sideOffset={6}
          collisionPadding={8}
          className="z-50 w-[min(21rem,calc(100vw-1rem))] rounded-xl border border-neutral-700 bg-neutral-900 text-sm text-neutral-100 shadow-2xl shadow-black/50 motion-safe:animate-sheet-in"
        >
          <header className="border-b border-neutral-800 px-4 pt-3 pb-2.5">
            <h2 className="font-semibold">Notifications</h2>
            <p className="mt-0.5 text-xs leading-4 text-neutral-400">When a session you are not looking at finishes its turn or needs you. Each one reaches you once.</p>
          </header>
          <div className="flex flex-col gap-1 p-1.5">
            <Choice label="Cards in the page" checked={notify.cards} onChange={(cards) => onChoose({ cards })} disabled={false}>
              While you are at Foundry: what Claude said or asks, to open or answer.
            </Choice>
            <Choice label="Desktop alerts" checked={desktopOn} onChange={(on) => onChoose({ desktop: on })} disabled={desktop === "blocked" || desktop === "unsupported"}>
              {blockedHint[desktop] ?? "While Foundry is in the background, instead of a card. Cleared when you come back."}
            </Choice>
            <Choice label="Catch-up when you come back" checked={notify.away} onChange={(away) => onChoose({ away })} disabled={false}>
              After two minutes away: what each session did meanwhile.
            </Choice>
          </div>
          <p className="border-t border-neutral-800 px-4 py-2.5 text-[11px] leading-4 text-neutral-500">Always on: the tab's title and icon show what needs you, and a screen reader hears each one.</p>
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}

function Choice({ label, checked, disabled, onChange, children }: { label: string; checked: boolean; disabled: boolean; onChange: (on: boolean) => void; children: ReactNode }) {
  return (
    <button
      type="button"
      role="switch"
      aria-label={label}
      aria-checked={checked}
      disabled={disabled}
      onClick={() => onChange(!checked)}
      className="group flex w-full items-start gap-3 rounded-lg px-2.5 py-2 text-left hover:bg-neutral-800/70 focus-visible:outline-2 focus-visible:outline-sky-400 disabled:hover:bg-transparent"
    >
      <span className="min-w-0 flex-1">
        <span className="block font-medium group-disabled:text-neutral-500">{label}</span>
        <span className="mt-0.5 block text-xs leading-4 text-neutral-400">{children}</span>
      </span>
      <span
        aria-hidden="true"
        className={`mt-0.5 flex h-5 w-9 shrink-0 items-center rounded-full p-0.5 transition-colors group-disabled:opacity-40 ${checked ? "justify-end bg-sky-400" : "justify-start bg-neutral-700"}`}
      >
        <span className={`size-4 rounded-full shadow ${checked ? "bg-neutral-950" : "bg-neutral-400"}`} />
      </span>
    </button>
  );
}
