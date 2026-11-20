import { useMutation } from "@tanstack/react-query";
import { useState, type ReactElement } from "react";
import { Dialog } from "radix-ui";
import { startSession } from "./api";
import { button, field, Modal } from "./Modal";

/**
 * "New session" for a workspace: asks for the model, then starts Claude Code in it. `trigger`
 * replaces the default text button.
 */
export function NewSession({ workspace, name, trigger = "New session" }: { workspace: string; name: string; trigger?: ReactElement | string }) {
  const [open, setOpen] = useState(false);
  const start = useMutation({
    mutationFn: (model: string) => startSession(workspace, model),
  });
  const change = (next: boolean) => {
    setOpen(next);
    start.reset();
  };

  return (
    <Modal open={open} onOpenChange={change} trigger={trigger} title={`New session in ${name}`} description="Starts Claude Code in the workspace.">
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
    </Modal>
  );
}
