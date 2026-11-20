import { useMutation } from "@tanstack/react-query";
import { useState } from "react";
import { Dialog } from "radix-ui";
import { button } from "./ArchiveButton";
import { field } from "./CreateWorkspace";
import { startSession } from "./api";

/** "New session" for a workspace: asks for the model, then starts Claude Code in it. */
export function NewSession({ workspace, name }: { workspace: string; name: string }) {
  const [open, setOpen] = useState(false);
  const start = useMutation({
    mutationFn: (model: string) => startSession(workspace, model),
  });
  const change = (next: boolean) => {
    setOpen(next);
    start.reset();
  };

  return (
    <Dialog.Root open={open} onOpenChange={change}>
      <Dialog.Trigger className={button}>New session</Dialog.Trigger>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 bg-black/60" />
        <Dialog.Content className="fixed left-1/2 top-1/2 w-96 -translate-x-1/2 -translate-y-1/2 rounded bg-neutral-900 p-4 text-sm text-neutral-100 shadow-xl">
          <Dialog.Title className="font-semibold">New session in {name}</Dialog.Title>
          <Dialog.Description className="text-neutral-400">Starts Claude Code in the workspace.</Dialog.Description>
          <form
            className="mt-3 space-y-3"
            onSubmit={(event) => {
              event.preventDefault();
              // Given per call, so a call the dialog was reset from cannot close it.
              start.mutate(String(new FormData(event.currentTarget).get("model")), { onSuccess: () => setOpen(false) });
            }}
          >
            <label className="block">
              Model
              <input name="model" className={field} defaultValue="opus" required />
            </label>
            {start.isError && (
              <p role="alert" className="text-red-400">
                {start.error.message}
              </p>
            )}
            <div className="flex justify-end gap-2">
              <Dialog.Close className={button}>Cancel</Dialog.Close>
              <button type="submit" className={button} disabled={start.isPending}>
                Start
              </button>
            </div>
          </form>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
