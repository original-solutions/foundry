import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { News } from "./AlertsBell";
import type { MachineView } from "./fleet";
import { needsYou } from "./labels";
import { fieldLook } from "./Modal";
import { arrive, current, TOAST_MS, type Live, type Toast } from "./news";
import { Now } from "./Now";
import { reply } from "./Said";
import { glyphText, StateGlyph } from "./StateGlyph";

/**
 * News from the sessions you are not looking at, while the page is in front: a session that
 * finishes its turn or comes to need you slides in a card at the top right (the top on a phone)
 * with what Claude said or asks, to open, or to answer on the spot when it rests on this machine,
 * without leaving the terminal you are in. A finished turn's card goes by itself after `TOAST_MS`
 * unless the pointer or the keyboard is on it; one that needs you stays while the session waits.
 * A card leaves once its session moves on or its terminal is opened. Which news becomes a card is
 * `useAlerts`' choice, so the same event never also raises a desktop alert. A screen reader hears
 * the same news from `Announcer`, so the cards are not a live region.
 */
export function Toasts({
  news,
  machines,
  here,
  onScreen,
  covered,
  onOpen,
}: {
  /** The news routed to cards, from `useAlerts`. */
  news: News | undefined;
  machines: MachineView[];
  /** This daemon's machine: only its sessions can be answered from here. */
  here: string | undefined;
  /** The terminals on screen, whose news needs no card. */
  onScreen: string[];
  /** Sessions the catch-up lists: their line there is live, so they get no card of their own. */
  covered: string[];
  onOpen: (terminal: string) => void;
}) {
  const [list, setList] = useState<Toast[]>([]);
  const latest = useRef({ machines, onScreen, covered });
  useLayoutEffect(() => {
    latest.current = { machines, onScreen, covered };
  });
  useEffect(() => {
    if (!news) return;
    const { machines, onScreen, covered } = latest.current;
    setList((old) => arrive(current(old, machines, onScreen).map((l) => l.toast), news.alerts.filter((a) => !covered.includes(a.tag)), onScreen));
  }, [news]);

  const live = current(list, machines, onScreen);
  if (live.length === 0) return null;
  const drop = (tag: string) => setList((old) => old.filter((t) => t.tag !== tag));
  return (
    <section aria-label="News from your sessions" className="flex flex-col gap-2">
      {live.map((l) => (
        <Card
          key={`${l.toast.tag}:${l.toast.glyph}`}
          live={l}
          canReply={l.row.glyph === "ready" && l.machine === here}
          onOpen={() => {
            drop(l.toast.tag);
            onOpen(l.toast.terminal);
          }}
          onGone={() => drop(l.toast.tag)}
        />
      ))}
    </section>
  );
}

function Card({ live, canReply, onOpen, onGone }: { live: Live; canReply: boolean; onOpen: () => void; onGone: () => void }) {
  const { toast, row } = live;
  const urgent = needsYou.includes(row.glyph);
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const [trouble, setTrouble] = useState(false);
  // Held while the pointer rests on the card, the keyboard is in it or a reply is begun, so a reply is never cut off.
  const [hovered, setHovered] = useState(false);
  const [focused, setFocused] = useState(false);
  const held = hovered || focused || draft !== "" || trouble;
  const gone = useRef(onGone);
  useLayoutEffect(() => {
    gone.current = onGone;
  });
  const left = useRef(TOAST_MS);
  useEffect(() => {
    if (urgent || held) return;
    const from = Date.now();
    const timer = setTimeout(() => gone.current(), left.current);
    return () => {
      clearTimeout(timer);
      left.current = Math.max(0, left.current - (Date.now() - from));
    };
  }, [urgent, held]);

  const send = async () => {
    const text = draft.trim();
    if (!text || sending) return;
    setSending(true);
    setTrouble(false);
    try {
      await reply(toast.terminal, text);
      onGone();
    } catch {
      setTrouble(true);
      setSending(false);
    }
  };

  return (
    <article
      aria-label={`${toast.title}: ${toast.body}`}
      data-urgent={urgent || undefined}
      onPointerEnter={() => setHovered(true)}
      onPointerLeave={() => setHovered(false)}
      onFocus={() => setFocused(true)}
      onBlur={(event) => {
        if (!event.currentTarget.contains(event.relatedTarget as Node | null)) setFocused(false);
      }}
      onKeyDown={(event) => {
        if (event.key !== "Escape") return;
        event.stopPropagation();
        onGone();
      }}
      className={`pointer-events-auto relative overflow-hidden rounded-xl border bg-neutral-900/95 text-sm text-neutral-100 shadow-2xl shadow-black/50 backdrop-blur motion-safe:animate-sheet-in ${
        urgent ? "border-amber-400/40" : "border-neutral-700/70"
      }`}
    >
      <div className="flex items-start gap-2.5 px-3 pt-2.5">
        <span className="mt-0.5">
          <StateGlyph glyph={row.glyph} />
        </span>
        <div className="min-w-0 flex-1">
          <p className="truncate font-semibold leading-5">{toast.title}</p>
          <p className="flex min-w-0 gap-1 text-xs leading-4">
            <span className={`shrink-0 ${glyphText[row.glyph]}`}>{toast.body}</span>
            {row.title && (
              <>
                <span aria-hidden="true" className="text-neutral-600">
                  ·
                </span>
                <span className="truncate text-neutral-500">{row.title}</span>
              </>
            )}
          </p>
        </div>
        <button type="button" aria-label="Dismiss" title="Dismiss (Esc)" onClick={onGone} className="-mt-0.5 -mr-1 rounded p-1 text-neutral-500 hover:bg-neutral-800 hover:text-neutral-200">
          <svg viewBox="0 0 16 16" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
            <path d="M4 4l8 8M12 4l-8 8" strokeLinecap="round" />
          </svg>
        </button>
      </div>
      {row.now ? (
        <Now step={row.now} className="mt-1.5 px-3 pl-[2.375rem] whitespace-normal" />
      ) : (
        row.said && <p className="mt-1.5 mr-3 ml-[2.375rem] line-clamp-4 border-l-2 border-sky-400/40 pl-2 text-xs leading-[1.125rem] whitespace-pre-line text-neutral-300">{row.said.replace(/\n{2,}/g, "\n")}</p>
      )}
      <form
        className="mt-2 flex items-center gap-1.5 px-3 pb-3 pl-[2.375rem]"
        onSubmit={(event) => {
          event.preventDefault();
          void send();
        }}
      >
        {canReply && (
          <>
            <input
              aria-label={`Reply to ${row.name}`}
              placeholder={`Reply to ${row.name}…`}
              value={draft}
              disabled={sending}
              enterKeyHint="send"
              onChange={(event) => setDraft(event.target.value)}
              className={`${fieldLook} h-8 min-w-0 flex-1 rounded-md px-2 text-base pointer-fine:h-7 pointer-fine:text-xs`}
            />
            <button
              type="submit"
              aria-label="Send"
              disabled={sending || !draft.trim()}
              className="grid size-8 shrink-0 place-items-center rounded-md bg-sky-400/15 text-sky-300 ring-1 ring-sky-400/40 hover:bg-sky-400/25 disabled:opacity-40 pointer-fine:size-7"
            >
              <svg viewBox="0 0 16 16" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                <path d="M3 8h10M9 4l4 4-4 4" />
              </svg>
            </button>
          </>
        )}
        <button
          type="button"
          onClick={onOpen}
          className={`h-8 shrink-0 rounded-md px-3 text-xs font-medium ring-1 pointer-fine:h-7 ${
            urgent ? "bg-amber-400/15 text-amber-300 ring-amber-400/40 hover:bg-amber-400/25" : "text-neutral-200 ring-neutral-600 hover:bg-neutral-800"
          } ${canReply ? "" : "ml-auto"}`}
        >
          Open
        </button>
      </form>
      {trouble && (
        <p role="alert" className="-mt-1.5 px-3 pb-2.5 pl-[2.375rem] text-xs text-amber-400">
          Could not reach {row.name}. Open its terminal to reply.
        </p>
      )}
      {!urgent && (
        <span
          aria-hidden="true"
          style={{ animationDuration: `${TOAST_MS}ms`, animationPlayState: held ? "paused" : "running" }}
          className="absolute inset-x-0 bottom-0 h-0.5 origin-left bg-sky-400/50 motion-safe:animate-drain motion-reduce:hidden"
        />
      )}
    </article>
  );
}
