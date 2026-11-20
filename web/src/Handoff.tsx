/** What a terminal pane shows while files are dragged over it: where they go, or why they cannot. */
export function DropOverlay({ name, count, refusal }: { name: string; count: number; refusal: string | null }) {
  const what = count > 1 ? `these ${count} files` : "this file";
  return (
    <div
      aria-live="polite"
      className={`pointer-events-none absolute inset-2 z-20 grid place-items-center rounded-lg border-2 border-dashed backdrop-blur-[2px] ${
        refusal ? "border-neutral-600 bg-neutral-950/85" : "border-sky-400/80 bg-neutral-950/80"
      }`}
    >
      <div className="flex max-w-sm flex-col items-center gap-3 px-6 text-center">
        <FileGlyph refused={!!refusal} />
        <p className={`text-lg font-semibold ${refusal ? "text-neutral-300" : "text-neutral-50"}`}>{refusal ?? `Drop to hand ${name} ${what}`}</p>
        <p className="text-sm text-neutral-400">
          {refusal
            ? "Open a terminal on this machine to hand it files."
            : "Kept beside this terminal on this machine, and its path typed in. Add your words and press Enter."}
        </p>
      </div>
    </div>
  );
}

/** A sheet with its corner folded, dropping into a prompt; crossed out when the drop is refused. */
function FileGlyph({ refused }: { refused: boolean }) {
  return (
    <svg viewBox="0 0 48 48" className={`size-14 ${refused ? "text-neutral-500" : "text-sky-400"}`} aria-hidden="true">
      <path d="M14 6h14l8 8v18a2 2 0 0 1-2 2H14a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2Z" fill="currentColor" fillOpacity="0.12" stroke="currentColor" strokeWidth="2" strokeLinejoin="round" />
      <path d="M28 6v8h8" fill="none" stroke="currentColor" strokeWidth="2" strokeLinejoin="round" />
      {refused ? (
        <path d="M17 15l14 14M31 15 17 29" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" />
      ) : (
        <path d="M24 14v12m-5-5 5 5 5-5" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round" className="motion-safe:animate-bounce" />
      )}
      <path d="M12 40l4 3-4 3M20 46h10" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" opacity="0.7" />
    </svg>
  );
}

/** The chip in a pane's corner while its files are on their way. */
export function SendingChip({ names }: { names: string[] }) {
  return (
    <div role="status" className="absolute right-4 bottom-4 z-20 flex items-center gap-2 rounded-full border border-sky-500/40 bg-neutral-900/95 px-3 py-1.5 text-xs text-neutral-200 shadow-lg shadow-black/40">
      <span className="size-2 rounded-full bg-sky-400 motion-safe:animate-pulse" aria-hidden="true" />
      Sending {names.length > 1 ? `${names.length} files` : names[0]}…
    </div>
  );
}
