import { useMutation } from "@tanstack/react-query";
import { useState, type ReactElement } from "react";
import { Dialog } from "radix-ui";
import { startSession } from "./api";
import { FirstPrompt, promptOf } from "./FirstPrompt";
import { button, field, Modal } from "./Modal";

/**
 * "New session" for a workspace: asks for the model, then starts Claude Code in it. `trigger`
 * replaces the default text button. Given `onClose` instead, the dialog has no trigger: it is
 * open from the start and `onClose` runs when it shuts. `onStarted` gets the new session's id.
 */
export function NewSession({
  workspace,
  name,
  trigger = "New session",
  onClose,
  onStarted,
}: {
  workspace: string;
  name: string;
  trigger?: ReactElement | string;
  onClose?: () => void;
  onStarted?: (session: string) => void;
}) {
  const [open, setOpen] = useState(!!onClose);
  const [prompt, setPrompt] = useState("");
  const start = useMutation({
    mutationFn: (input: { model: string; prompt?: string }) => startSession(workspace, input.model, input.prompt),
  });
  const change = (next: boolean) => {
    setOpen(next);
    if (next) setPrompt("");
    else onClose?.();
    start.reset();
  };

  return (
    <Modal open={open} onOpenChange={change} trigger={onClose ? undefined : trigger} title={`New session in ${name}`} description="Starts Claude Code in the workspace, on your first prompt if you write one.">
      <form
        className="mt-3 space-y-3"
        onSubmit={(event) => {
          event.preventDefault();
          // Given per call, so a call the dialog was reset from cannot close it.
          const model = String(new FormData(event.currentTarget).get("model"));
          start.mutate({ model, prompt: promptOf(prompt) }, {
              onSuccess: (session) => {
                onStarted?.(session);
                change(false);
              },
            });
        }}
      >
        <label className="block">
          Model
          <input name="model" className={field} defaultValue="opus" required />
        </label>
        <FirstPrompt id={`first-prompt-${workspace}`} value={prompt} onChange={setPrompt} />
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
