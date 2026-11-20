import { useState } from "react";
import { fieldLook } from "./Modal";
import { reply } from "./reply";

/** A reply being written to the session in `terminal`: its draft, and `send`, which types it in and calls `onSent`. */
export function useReply(terminal: string, onSent: () => void) {
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const [trouble, setTrouble] = useState(false);
  const send = async () => {
    const text = draft.trim();
    if (!text || sending) return;
    setSending(true);
    setTrouble(false);
    try {
      await reply(terminal, text);
      setDraft("");
      onSent();
    } catch {
      setTrouble(true);
    } finally {
      setSending(false);
    }
  };
  return { draft, setDraft, sending, trouble, setTrouble, send };
}

/** What a reply that could not reach session `name` says. */
export const unreached = (name: string) => `Could not reach ${name}. Open its terminal to reply.`;

/**
 * The box a reply to session `name` is written in and its send button, for a form whose submit
 * calls `reply.send`. Escape calls `onEscape` and goes no further.
 */
export function ReplyBox({ reply, name, autoFocus, onEscape }: { reply: ReturnType<typeof useReply>; name: string; autoFocus?: boolean; onEscape?: () => void }) {
  return (
    <>
      <input
        autoFocus={autoFocus}
        aria-label={`Reply to ${name}`}
        placeholder={`Reply to ${name}…`}
        value={reply.draft}
        disabled={reply.sending}
        enterKeyHint="send"
        onChange={(e) => reply.setDraft(e.target.value)}
        onKeyDown={(e) => {
          if (e.key !== "Escape" || !onEscape) return;
          e.stopPropagation();
          onEscape();
        }}
        className={`${fieldLook} h-8 min-w-0 flex-1 rounded-md px-2 text-base pointer-fine:h-7 pointer-fine:text-xs`}
      />
      <button
        type="submit"
        aria-label="Send"
        disabled={reply.sending || !reply.draft.trim()}
        className="grid size-8 shrink-0 place-items-center rounded-md bg-sky-400/15 text-sky-300 ring-1 ring-sky-400/40 hover:bg-sky-400/25 disabled:opacity-40 pointer-fine:size-7"
      >
        <svg viewBox="0 0 16 16" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
          <path d="M3 8h10M9 4l4 4-4 4" />
        </svg>
      </button>
    </>
  );
}

/**
 * What Claude said at the end of its last turn, under the session's row: three lines until it is
 * opened, then all of it, with a box to answer it. A reply goes into the session's terminal as if
 * typed there, so the next turn starts without leaving the board.
 */
export function Said({ said, name, terminal, canReply }: { said: string; name: string; terminal: string; canReply: boolean }) {
  const [open, setOpen] = useState(false);
  const answer = useReply(terminal, () => close());
  function close() {
    setOpen(false);
    answer.setTrouble(false);
  }
  return (
    <div data-said className="mt-0.5 mb-1 ml-[1.625rem] mr-1">
      <button
        type="button"
        aria-expanded={open}
        title={open ? undefined : canReply ? "Read it all and reply" : "Read it all"}
        onClick={() => (open ? close() : setOpen(true))}
        className={`w-full cursor-pointer rounded-r-md border-l-2 border-sky-400/40 py-0.5 pr-1 pl-2 text-left text-xs leading-[1.125rem] whitespace-pre-line text-neutral-300 hover:border-sky-400/80 hover:bg-neutral-800/50 ${
          open ? "block max-h-64 overflow-y-auto" : "line-clamp-3"
        }`}
      >
        {open ? said : said.replace(/\n{2,}/g, "\n")}
      </button>
      {open && canReply && (
        <form
          className="mt-1.5 flex items-center gap-1.5"
          onSubmit={(e) => {
            e.preventDefault();
            void answer.send();
          }}
        >
          <ReplyBox reply={answer} name={name} autoFocus onEscape={close} />
        </form>
      )}
      {answer.trouble && (
        <p role="alert" className="mt-1 text-xs text-amber-400">
          {unreached(name)}
        </p>
      )}
    </div>
  );
}
