import { useEffect, useState } from "react";
import type { SessionView } from "./fleet";
import { button, primaryButton } from "./Modal";
import { review, takers, type Note } from "./notes";
import { busy, plural } from "./labels";
import { reply } from "./Said";
import { StateGlyph } from "./StateGlyph";

/** What a comment's row, box and buttons are called: the line it is on, and whether that line was removed. */
export const lineName = (note: Pick<Note, "side" | "line">) => (note.side === "old" ? `removed line ${note.line}` : `line ${note.line}`);

/** The box a comment is written in, under its line. Cmd or Ctrl+Enter adds it; Escape (handled by the sheet) drops it. */
export function NoteBox({ name, initial, onSave, onCancel }: { name: string; initial: string; onSave: (body: string) => void; onCancel: () => void }) {
  const [body, setBody] = useState(initial);
  const save = () => body.trim() && onSave(body.trim());
  return (
    <div role="row" aria-label={`Comment on ${name}`} className="sticky left-0 w-[min(100%,48rem)] px-3 py-2 font-sans md:pl-24">
      <div role="cell" className="rounded-md border border-sky-400/40 bg-neutral-950 p-2 shadow-lg shadow-black/30">
        <textarea
          autoFocus
          aria-label={`Comment on ${name}`}
          placeholder="Tell Claude what to change here…"
          rows={3}
          value={body}
          onChange={(e) => setBody(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
              e.preventDefault();
              save();
            }
          }}
          className="block w-full resize-y rounded border border-neutral-700 bg-neutral-900 px-2 py-1.5 text-base text-neutral-100 placeholder:text-neutral-500 focus:border-sky-400/70 focus:outline-none pointer-fine:text-xs"
        />
        <div className="mt-2 flex items-center justify-end gap-2">
          <span className="mr-auto hidden text-[11px] text-neutral-500 pointer-fine:inline">⌘↵ to add · Esc to cancel</span>
          <button type="button" onClick={onCancel} className={button}>
            Cancel
          </button>
          <button type="button" onClick={save} disabled={!body.trim()} className={primaryButton}>
            {initial ? "Save" : "Add comment"}
          </button>
        </div>
      </div>
    </div>
  );
}

/** A comment waiting to be sent, under its line: click it to change it. `moved` says its line has since changed. */
export function NoteRow({ note, moved, onEdit, onDelete }: { note: Note; moved?: boolean; onEdit: () => void; onDelete: () => void }) {
  const name = lineName(note);
  return (
    <div role="row" aria-label={`Comment on ${name}`} className="sticky left-0 w-[min(100%,48rem)] px-3 py-1.5 font-sans md:pl-24">
      <div role="cell" className="flex items-start gap-2 rounded-md border-l-2 border-sky-400 bg-sky-400/10 py-1.5 pr-1 pl-2.5">
        <button type="button" onClick={onEdit} title="Change this comment" className="min-w-0 flex-1 cursor-text text-left text-xs leading-5 whitespace-pre-wrap text-neutral-100">
          <span className="mb-0.5 block text-[11px] font-medium text-sky-300">
            For Claude{moved && <span className="font-normal text-amber-300"> · on {name}, which has changed since</span>}
          </span>
          {note.body}
        </button>
        <button type="button" aria-label={`Delete comment on ${name}`} onClick={onDelete} className="rounded p-1 text-neutral-500 hover:bg-neutral-800 hover:text-neutral-100">
          <svg viewBox="0 0 16 16" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
            <path d="M4 4l8 8M12 4l-8 8" strokeLinecap="round" />
          </svg>
        </button>
      </div>
    </div>
  );
}

/** How long "Sent" stays at the foot of the sheet once the session that took the comments is not busy. */
const SENT_FOR = 8000;

/**
 * The foot of the sheet while there are comments: how many, which session takes them, and Send,
 * which types them into that session as one message and presses Enter. Only an idle session can
 * take them, so a turn in progress or a question on screen is never answered by accident.
 */
export function SendBar({ notes, sessions, onSent }: { notes: Note[]; sessions: SessionView[]; onSent: () => void }) {
  const list = takers(sessions);
  const [picked, setPicked] = useState<string>();
  const [sending, setSending] = useState(false);
  const [trouble, setTrouble] = useState(false);
  const [sent, setSent] = useState<{ count: number; terminal: string }>();
  const taker = list.find((t) => t.terminal === picked && t.ready) ?? list.find((t) => t.ready) ?? list[0];
  const took = sent && list.find((t) => t.terminal === sent.terminal);
  const working = took !== undefined && busy.includes(took.glyph);

  useEffect(() => {
    if (!sent || working) return;
    const done = setTimeout(() => setSent(undefined), SENT_FOR);
    return () => clearTimeout(done);
  }, [sent, working]);

  const send = async () => {
    if (!taker?.ready || sending) return;
    setSending(true);
    setTrouble(false);
    try {
      await reply(taker.terminal, review(notes));
      setSent({ count: notes.length, terminal: taker.terminal });
      onSent();
    } catch {
      setTrouble(true);
    } finally {
      setSending(false);
    }
  };

  if (notes.length === 0)
    return sent && took ? (
      <footer
        role="status"
        className="flex shrink-0 flex-wrap items-center gap-x-2 gap-y-1 border-t border-neutral-800 bg-neutral-900/90 px-4 py-2.5 pb-[max(0.625rem,env(safe-area-inset-bottom))] text-xs"
      >
        <span className="text-emerald-300">
          Sent {plural(sent.count, "comment")} to {took.name}.
        </span>
        {working ? (
          <span className="flex items-center gap-1.5 text-neutral-300">
            <StateGlyph glyph={took.glyph} />
            {took.name} is {took.glyph === "working" ? "working on it" : took.label.toLowerCase()}; its edits show here as it makes them.
          </span>
        ) : (
          <span className="text-neutral-400">Its edits show here as it makes them.</span>
        )}
      </footer>
    ) : null;

  const why = !taker ? "No session in this workspace to take them." : !taker.ready ? `${taker.name} is ${taker.label.toLowerCase()}. Send when it is ready.` : undefined;
  return (
    <footer className="flex shrink-0 flex-wrap items-center gap-x-3 gap-y-2 border-t border-sky-400/30 bg-neutral-900/95 px-4 py-2.5 pb-[max(0.625rem,env(safe-area-inset-bottom))] text-xs motion-safe:animate-sheet-in">
      <span className="flex items-center gap-2 text-neutral-200">
        <span aria-hidden="true" className="grid size-5 place-items-center rounded-full bg-sky-400 text-[11px] font-semibold text-neutral-950">
          {notes.length}
        </span>
        {plural(notes.length, "comment")} for Claude
      </span>
      {list.length > 1 && (
        <label className="flex min-w-0 items-center gap-1.5 text-neutral-400">
          to
          <select
            value={taker?.terminal}
            onChange={(e) => setPicked(e.target.value)}
            className="max-w-56 min-w-0 truncate rounded border border-neutral-700 bg-neutral-950 px-1.5 py-0.5 text-neutral-100"
          >
            {list.map((t) => (
              <option key={t.terminal} value={t.terminal} disabled={!t.ready}>
                {t.ready ? t.name : `${t.name} (${t.label.toLowerCase()})`}
              </option>
            ))}
          </select>
        </label>
      )}
      {trouble ? (
        <span role="alert" className="text-amber-300">
          Could not reach {taker?.name}. Your comments are kept.
        </span>
      ) : (
        why && <span className="text-neutral-500">{why}</span>
      )}
      <button type="button" onClick={() => void send()} disabled={!taker?.ready || sending} className={`ml-auto ${primaryButton} px-3 py-1`}>
        {sending ? "Sending…" : `Send ${plural(notes.length, "comment")} to ${taker?.name ?? "Claude"}`}
      </button>
    </footer>
  );
}
