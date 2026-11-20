import { useState, type ReactNode } from "react";
import { keyData, keysFor } from "./keys";
import type { Glyph } from "./labels";
import { fieldLook } from "./Modal";

/**
 * The touch screen's way to drive a terminal: a row of the keys a phone keyboard lacks (with
 * one-tap answers while a session waits for you) over a box to write a message in, which goes
 * as one paste and then Enter. Hidden where the main pointer is a mouse: there is a keyboard.
 * A key press keeps focus where it was, so the on-screen keyboard neither rises nor drops.
 * With `onFiles`, a clip beside the box picks files (a photo, a screenshot) to hand the terminal.
 * `lead` goes first in the row: an idle session's next steps.
 */
export function KeyBar({
  name,
  glyph,
  modes,
  onKey,
  onMessage,
  onFiles,
  lead,
}: {
  name: string;
  glyph: Glyph;
  modes: () => { applicationCursorKeysMode: boolean };
  onKey: (data: string) => void;
  onMessage: (text: string) => void;
  onFiles?: (files: File[]) => void;
  lead?: ReactNode;
}) {
  const [draft, setDraft] = useState("");
  const shell = name === "shell";
  const send = () => {
    if (!draft) return;
    onMessage(draft);
    setDraft("");
  };
  const keys = keysFor(glyph);
  return (
    <div className="shrink-0 border-t border-neutral-800 bg-neutral-950 pb-[env(safe-area-inset-bottom)] pointer-fine:hidden">
      <div role="toolbar" aria-label={`Keys for ${name}`} className="flex items-center gap-1 overflow-x-auto px-2 pt-2 [scrollbar-width:none]">
        {lead}
        {keys.some((k) => k.answer) && <span className="shrink-0 pr-0.5 text-[10px] font-medium tracking-wide text-amber-400 uppercase">Answer</span>}
        {keys.map((key) => (
          <button
            key={key.label}
            type="button"
            aria-label={key.title}
            title={key.title}
            onPointerDown={(e) => e.preventDefault()}
            onClick={() => onKey(keyData(key, modes()))}
            className={`h-9 min-w-9 flex-1 shrink-0 rounded-md px-1.5 font-mono text-sm active:scale-95 ${
              key.answer
                ? "bg-amber-400/15 font-semibold text-amber-300 ring-1 ring-amber-400/50 active:bg-amber-400/30"
                : "bg-neutral-800/80 text-neutral-200 active:bg-neutral-700"
            }`}
          >
            {key.label}
          </button>
        ))}
      </div>
      <form
        className="flex items-center gap-2 px-2 py-2"
        onSubmit={(e) => {
          e.preventDefault();
          send();
        }}
      >
        {onFiles && (
          <label title="Attach files" className="grid size-10 shrink-0 cursor-pointer place-items-center rounded-full bg-neutral-800/80 text-neutral-300 active:bg-neutral-700">
            <input
              type="file"
              multiple
              aria-label={`Attach files for ${name}`}
              className="sr-only"
              onChange={(e) => {
                onFiles(Array.from(e.target.files ?? []));
                e.target.value = "";
              }}
            />
            <svg viewBox="0 0 16 16" className="size-4" aria-hidden="true">
              <path d="M10.5 4.5 5.8 9.2a1.4 1.4 0 0 0 2 2l5-5a2.8 2.8 0 0 0-4-4l-5 5a4.2 4.2 0 0 0 6 6l4.2-4.2" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
            </svg>
          </label>
        )}
        <input
          aria-label={`Message ${name}`}
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.nativeEvent.isComposing) {
              e.preventDefault();
              send();
            }
          }}
          placeholder={shell ? "Type a command…" : `Message ${name}…`}
          enterKeyHint="send"
          autoCapitalize={shell ? "off" : "sentences"}
          autoCorrect={shell ? "off" : "on"}
          spellCheck={!shell}
          className={`${fieldLook} h-10 min-w-0 flex-1 rounded-full px-4 text-[16px]`}
        />
        <button
          type="submit"
          aria-label="Send"
          disabled={!draft}
          onPointerDown={(e) => e.preventDefault()}
          className="grid size-10 shrink-0 place-items-center rounded-full bg-sky-400 text-neutral-950 transition-colors disabled:bg-neutral-800 disabled:text-neutral-500"
        >
          <svg viewBox="0 0 16 16" className="size-4" aria-hidden="true">
            <path d="M8 13V3M3.5 7.5 8 3l4.5 4.5" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
        </button>
      </form>
    </div>
  );
}
