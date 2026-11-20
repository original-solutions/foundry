import { jumpKey } from "./JumpPalette";

/** The chord that sends a form from the first-prompt box: Enter alone writes a new line. */
export const sendKey = jumpKey === "⌘K" ? "⌘ Enter" : "Ctrl Enter";

/** The prompt to send, or `undefined` when nothing but space was written. */
export function promptOf(text: string): string | undefined {
  const trimmed = text.trim();
  return trimmed === "" ? undefined : trimmed;
}

/**
 * The box a new session's first prompt is written in: Claude Code starts on it, so you land on
 * an agent already working instead of an empty prompt. Grows with what is written; Cmd+Enter
 * (Ctrl+Enter elsewhere) sends the form it is in.
 */
export function FirstPrompt({ id, value, onChange }: { id: string; value: string; onChange: (value: string) => void }) {
  return (
    <div className="group relative rounded-lg border border-neutral-700 bg-neutral-950 focus-within:border-neutral-400">
      <div className="flex items-center gap-1.5 px-3 pt-2 text-xs text-neutral-400">
        <SparkIcon className={`size-3.5 ${value.trim() ? "text-emerald-400" : "text-neutral-500"}`} />
        <label htmlFor={id}>First prompt</label>
        <span className="text-neutral-500">· optional</span>
      </div>
      <textarea
        id={id}
        name="prompt"
        rows={3}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
            e.preventDefault();
            e.currentTarget.form?.requestSubmit();
          }
        }}
        placeholder="What should Claude do first? It starts on this the moment it opens."
        className="block max-h-56 min-h-20 w-full resize-none bg-transparent px-3 pt-1 pb-7 text-base leading-relaxed sm:text-sm text-neutral-100 outline-none [field-sizing:content] placeholder:text-neutral-600"
      />
      <kbd className="pointer-events-none absolute right-2 bottom-2 pointer-coarse:hidden rounded border border-neutral-700 px-1.5 py-0.5 text-[10px] text-neutral-500 opacity-0 transition-opacity group-focus-within:opacity-100">
        {sendKey}
      </kbd>
    </div>
  );
}

export function SparkIcon({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 16 16" className={className} fill="currentColor" aria-hidden="true">
      <path d="M8 1.5c.3 2.9 1.6 4.2 4.5 4.5-2.9.3-4.2 1.6-4.5 4.5-.3-2.9-1.6-4.2-4.5-4.5 2.9-.3 4.2-1.6 4.5-4.5ZM12.5 10c.15 1.3.7 1.85 2 2-1.3.15-1.85.7-2 2-.15-1.3-.7-1.85-2-2 1.3-.15 1.85-.7 2-2Z" />
    </svg>
  );
}
