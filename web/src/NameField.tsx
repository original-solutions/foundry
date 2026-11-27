import { useEffect, useRef } from "react";
import { fieldLook } from "./Modal";

/**
 * A name edited in place, a machine's or a session's: it opens on its text with all of it
 * selected. Enter calls `onSave`, Escape `onCancel`, and leaving the field `onLeave` when given.
 * Every key stays in the field, so a list's own keys (the sidebar's arrows and type-ahead) never
 * move away from it. While the name is on its way the field is busy and takes no edits. A refusal
 * shows under the field, or beside it when `inline`, where there is no room under it, and the
 * text stays. `className` sizes the field's text to the name it stands in for.
 *
 * A terminal opened with the field (Rename on one that was not open) takes the keyboard as it
 * shows, after the field did. The field takes it back once the page settles, and a leave before
 * then is not the person's, so it calls nothing.
 */
export function NameField({
  label,
  text,
  onText,
  busy,
  error,
  onSave,
  onCancel,
  onLeave,
  placeholder,
  inline = false,
  className = "",
}: {
  label: string;
  text: string;
  onText: (text: string) => void;
  busy: boolean;
  error?: string;
  onSave: () => void;
  onCancel: () => void;
  onLeave?: () => void;
  placeholder?: string;
  inline?: boolean;
  className?: string;
}) {
  const input = useRef<HTMLInputElement>(null);
  const settled = useRef(false);
  useEffect(() => {
    const timer = setTimeout(() => {
      input.current?.focus();
      settled.current = true;
    });
    return () => clearTimeout(timer);
  }, []);
  return (
    <div className={`min-w-0 flex-1 ${inline ? "flex items-center gap-2" : ""}`}>
      <input
        ref={input}
        aria-label={label}
        autoFocus
        autoComplete="off"
        spellCheck={false}
        value={text}
        placeholder={placeholder}
        readOnly={busy}
        aria-busy={busy || undefined}
        onFocus={(e) => e.currentTarget.select()}
        onChange={(e) => onText(e.target.value)}
        onBlur={() => settled.current && onLeave?.()}
        onKeyDown={(e) => {
          e.stopPropagation();
          if (e.key === "Enter") {
            e.preventDefault();
            onSave();
          } else if (e.key === "Escape") {
            e.preventDefault();
            onCancel();
          }
        }}
        className={`w-full min-w-0 rounded px-1 py-px aria-busy:opacity-60 ${fieldLook} ${className}`}
      />
      {error && (
        <p role="alert" title={inline ? error : undefined} className={`text-xs font-normal text-red-400 ${inline ? "max-w-1/2 shrink-0 truncate" : "mt-1"}`}>
          {error}
        </p>
      )}
    </div>
  );
}
