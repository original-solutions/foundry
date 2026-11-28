import { useMutation } from "@tanstack/react-query";
import { useState } from "react";
import { renameSession } from "./api";
import { NameField } from "./NameField";
import { useTerminalActions, type Place } from "./TerminalMenu";

/**
 * Whether the name of the session in terminal `terminal` is being edited at `place`, with what
 * starts editing it there (a double-click on the name) and what stops it. `undefined` outside
 * `TerminalActions`, where nothing is renamed.
 */
export function useRenaming(terminal: string, place: Place) {
  const at = useTerminalActions();
  if (!at) return undefined;
  return {
    editing: at.renaming?.terminal === terminal && at.renaming.place === place,
    start: () => at.rename(terminal, place),
    stop: at.stopRenaming,
  };
}

/**
 * Session `session`'s name edited in place, from `name`, what it shows now: the name it was
 * given, else its first prompt, else nothing. Enter sends the text, trimmed, and closes the field
 * once the daemon takes it; the name shows once the owner's record of it arrives, on every
 * machine. An empty field clears the name, so the first prompt shows again. The name it already
 * shows sends nothing, so a first prompt is never kept as a name by a stray Enter. Escape closes
 * it and sends nothing; leaving the field sends a changed name and closes an unchanged one.
 */
export function SessionNameField({ session, name, onClose, inline, className }: { session: string; name: string; onClose: () => void; inline?: boolean; className?: string }) {
  const [text, setText] = useState(name);
  const rename = useMutation({ mutationFn: (next: string) => renameSession(session, next), onSuccess: onClose });
  const changed = text.trim() !== name.trim();
  const save = () => {
    if (!changed) return onClose();
    if (!rename.isPending) rename.mutate(text.trim());
  };
  return (
    <NameField
      label="Session name"
      placeholder="Name this session"
      text={text}
      onText={setText}
      busy={rename.isPending}
      error={rename.isError ? rename.error.message : undefined}
      onSave={save}
      onCancel={onClose}
      // A refused name stays in the field with its reason until Enter or Escape.
      onLeave={() => {
        if (!changed) onClose();
        else if (!rename.isError) save();
      }}
      inline={inline}
      className={className}
    />
  );
}
