import { useEffect, useState } from "react";
import { button, primaryButton } from "./Modal";
import { heardAgo } from "./samples";
import { shapes } from "./StateGlyph";
import { useNow } from "./StepsPanel";
import { ThemeButton } from "./ThemeButton";
import type { Link } from "./useFleet";

/** How long the "back" line stays after the link returns. */
const BACK_FOR = 4000;

/** The wait until the next try, or that one is under way. */
function nextTry(retryAt: number | undefined, now: number, lead: string): string {
  const seconds = retryAt === undefined ? 0 : Math.ceil((retryAt - now) / 1000);
  return seconds > 0 ? `${lead} ${seconds} s` : "Trying…";
}

/** A stretch of time in a few words: "14 s", "3 min". */
function lasted(ms: number): string {
  const seconds = Math.max(1, Math.round(ms / 1000));
  return seconds < 60 ? `${seconds} s` : `${Math.round(seconds / 60)} min`;
}

/** The away glyph's nine dots, large: only the corners lit. The centre beats while a try is under way. */
function Beacon({ trying }: { trying: boolean }) {
  return (
    <svg viewBox="0 0 13 13" aria-hidden="true" className="size-16 text-neutral-500">
      {[...shapes.away].map((cell, i) => (
        <circle
          key={i}
          cx={2 + (i % 3) * 4.5}
          cy={2 + Math.floor(i / 3) * 4.5}
          r={1.6}
          fill="currentColor"
          opacity={cell === "#" ? 1 : 0.18}
          className={i === 4 && trying ? "text-amber-400 motion-safe:animate-ripple" : undefined}
        />
      ))}
    </svg>
  );
}

/**
 * The whole page while the daemon has never sent its state: where the page looked, how often it
 * tried, a count down to the next try and a button to try now. It stands in for the board and the
 * sidebar, which would otherwise show an empty fleet as if there were no work.
 */
export function Unreachable({ link }: { link: Link }) {
  const now = useNow(1000);
  const over = lasted(now - (link.lostAt ?? now));
  const trying = nextTry(link.retryAt, now, "Next try in");
  return (
    <div className="flex h-dvh flex-col bg-neutral-950 text-neutral-100">
      <div className="flex items-center justify-between px-4 py-3">
        <span className="text-lg font-semibold">Foundry</span>
        <ThemeButton />
      </div>
      <main className="flex flex-1 items-center justify-center px-4 pb-[10vh]">
        <div className="flex w-full max-w-md flex-col items-start gap-5">
          <Beacon trying={trying === "Trying…"} />
          <div className="space-y-3">
            <h1 className="text-2xl font-semibold tracking-tight">Can't reach the daemon</h1>
            <p className="text-sm leading-relaxed text-neutral-400">
              This page gets every workspace, session and shell from <code className="font-mono text-neutral-200">foundryd</code> at{" "}
              <code className="font-mono text-neutral-200">{location.host}</code>, and it has not answered. Nothing is shown until it does, so this is not an
              empty fleet: terminals keep running on their machine whether or not this page can see them.
            </p>
            <p className="text-sm leading-relaxed text-neutral-500">Check that foundryd is running on that machine; if it is, its log says why it is not answering.</p>
          </div>
          <div className="flex w-full flex-wrap items-center gap-x-4 gap-y-2 rounded-lg border border-neutral-800 bg-neutral-900 px-4 py-3 text-sm">
            <span className="text-neutral-400">{link.tries <= 1 ? "Tried once" : `Tried ${link.tries} times over ${over}`}</span>
            <span className="text-amber-400 tabular-nums">{trying}</span>
            <button type="button" onClick={link.retry} className={`${primaryButton} ml-auto`}>
              Try now
            </button>
          </div>
        </div>
      </main>
    </div>
  );
}

/** The line across the top while the link is lost: since when, the next try, and Try now. */
function Lost({ link }: { link: Link }) {
  const now = useNow(1000);
  return (
    <div role="status" className="flex shrink-0 flex-wrap items-center gap-x-3 gap-y-1 border-b border-amber-400/30 bg-amber-400/10 px-4 py-2 text-sm text-amber-200">
      <span className="min-w-0 flex-1">
        <span className="font-medium">Lost the daemon {heardAgo(link.lostAt, now) ?? "just now"}.</span>{" "}
        <span className="hidden text-amber-200/70 sm:inline">Below is what it last said: states and numbers are frozen until it is back.</span>
      </span>
      <span className="text-xs tabular-nums">{nextTry(link.retryAt, now, "Retrying in")}</span>
      <button type="button" onClick={link.retry} className={`${button} border-amber-400/40 text-amber-100 hover:bg-amber-400/15`}>
        Try now
      </button>
    </div>
  );
}

/**
 * Across the top of the page once it has had the daemon's state: while the link is lost, that what
 * is shown is the daemon's last word and when it is tried next; when it comes back, once, for how
 * long it was gone.
 */
export function LinkBanner({ link }: { link: Link }) {
  /** When the "back" line last shown came back, once its time is up. */
  const [shown, setShown] = useState<number>();
  const back = link.back;
  useEffect(() => {
    if (!back) return;
    const timer = setTimeout(() => setShown(back.at), BACK_FOR);
    return () => clearTimeout(timer);
  }, [back]);

  if (link.lostAt !== undefined) return <Lost link={link} />;
  if (!back || back.at === shown) return null;
  return (
    <div role="status" className="flex shrink-0 items-center border-b border-emerald-400/30 bg-emerald-400/10 px-4 py-2 text-sm text-emerald-200">
      <span className="font-medium">Back after {lasted(back.after)}.</span>&nbsp;<span className="hidden text-emerald-200/70 sm:inline">Everything is live again.</span>
    </div>
  );
}
