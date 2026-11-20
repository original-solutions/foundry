import { useState } from "react";
import { socketUrl } from "./socket";

/**
 * Types `text` into terminal `id` as one paste and presses Enter, over a socket of its own that
 * closes once both are sent. It never resizes the terminal, so a pane showing it is left alone.
 * Rejects when the terminal cannot be reached.
 */
export function reply(id: string, text: string): Promise<void> {
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(socketUrl(`/api/terminals/${id}`));
    socket.binaryType = "arraybuffer";
    const bytes = new TextEncoder();
    socket.onopen = () => {
      socket.send(bytes.encode(`\x1b[200~${text}\x1b[201~`));
      socket.send(bytes.encode("\r"));
      socket.close(1000);
      resolve();
    };
    socket.onclose = (event) => reject(new Error(event.reason || "the terminal closed"));
  });
}

/**
 * What Claude said at the end of its last turn, under the session's row: three lines until it is
 * opened, then all of it, with a box to answer it. A reply goes into the session's terminal as if
 * typed there, so the next turn starts without leaving the board.
 */
export function Said({ said, name, terminal, canReply }: { said: string; name: string; terminal: string; canReply: boolean }) {
  const [open, setOpen] = useState(false);
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const [trouble, setTrouble] = useState(false);
  const close = () => {
    setOpen(false);
    setTrouble(false);
  };
  const send = async () => {
    const text = draft.trim();
    if (!text || sending) return;
    setSending(true);
    setTrouble(false);
    try {
      await reply(terminal, text);
      setDraft("");
      close();
    } catch {
      setTrouble(true);
    } finally {
      setSending(false);
    }
  };
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
            void send();
          }}
        >
          <input
            autoFocus
            aria-label={`Reply to ${name}`}
            placeholder={`Reply to ${name}…`}
            value={draft}
            disabled={sending}
            enterKeyHint="send"
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key !== "Escape") return;
              e.stopPropagation();
              close();
            }}
            className="h-8 min-w-0 flex-1 rounded-md border border-neutral-700 bg-neutral-950 px-2 text-base pointer-fine:h-7 pointer-fine:text-xs text-neutral-100 placeholder:text-neutral-500 focus:border-sky-400/70 focus:outline-none"
          />
          <button
            type="submit"
            aria-label="Send"
            disabled={sending || !draft.trim()}
            className="grid size-7 shrink-0 place-items-center rounded-md bg-sky-400/15 text-sky-300 ring-1 ring-sky-400/40 hover:bg-sky-400/25 disabled:opacity-40"
          >
            <svg viewBox="0 0 16 16" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
              <path d="M3 8h10M9 4l4 4-4 4" />
            </svg>
          </button>
        </form>
      )}
      {trouble && (
        <p role="alert" className="mt-1 text-xs text-amber-400">
          Could not reach {name}. Open its terminal to reply.
        </p>
      )}
    </div>
  );
}
