import { useMutation } from "@tanstack/react-query";
import { useState } from "react";
import { Dialog } from "radix-ui";
import { button } from "./ArchiveButton";
import { ApiError, createWorkspace, startSession, type Project } from "./api";

export const field = "mt-1 w-full rounded border border-neutral-600 bg-neutral-950 px-2 py-1 text-sm";

/** The workspace was made, but its session did not start. */
class SessionNotStarted extends ApiError {}

/** The "New workspace" button and its dialog: makes the workspace, then starts a session in it. */
export function CreateWorkspace({ projects }: { projects: Project[] }) {
  const [open, setOpen] = useState(false);
  const [project, setProject] = useState("");
  const create = useMutation({
    mutationFn: async (input: { project: string; base: string; name: string; model: string }) => {
      const workspace = await createWorkspace({ project: input.project, base: input.base, name: input.name });
      // The workspace is usable when its setup did not start, so its session is started all the same.
      try {
        await startSession(workspace.id, input.model);
      } catch (error) {
        const setup = workspace.error ? ` Also: ${workspace.error}` : "";
        throw new SessionNotStarted(`The workspace was made, but its session did not start: ${error instanceof Error ? error.message : error}.${setup}`);
      }
      return workspace.error ?? null;
    },
    onSuccess: (warning) => {
      if (!warning) setOpen(false);
    },
  });
  const warning = create.data ?? null;
  // Once the workspace exists, a second create would only find its name taken, so the dialog
  // then offers Close alone, with what went wrong. Read from the mutation, which a reset
  // detaches from a call still on its way.
  const made = create.isSuccess || create.error instanceof SessionNotStarted;

  const change = (next: boolean) => {
    setOpen(next);
    create.reset();
  };

  return (
    <Dialog.Root open={open} onOpenChange={change}>
      <Dialog.Trigger className={button}>New workspace</Dialog.Trigger>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 bg-black/60" />
        <Dialog.Content className="fixed left-1/2 top-1/2 w-96 -translate-x-1/2 -translate-y-1/2 rounded bg-neutral-900 p-4 text-sm text-neutral-100 shadow-xl">
          <Dialog.Title className="font-semibold">New workspace</Dialog.Title>
          <Dialog.Description className="text-neutral-400">Makes a branch and starts a session in it.</Dialog.Description>
          <form
            className="mt-3 space-y-3"
            onSubmit={(event) => {
              event.preventDefault();
              const form = new FormData(event.currentTarget);
              create.mutate({
                project: String(form.get("project")),
                base: String(form.get("base")),
                name: String(form.get("name")),
                model: String(form.get("model")),
              });
            }}
          >
            {projects.length === 0 ? (
              <p>No projects yet</p>
            ) : (
              <label className="block">
                Project
                <select name="project" className={field} value={project || projects[0].id} onChange={(e) => setProject(e.target.value)}>
                  {projects.map((p) => (
                    <option key={p.id} value={p.id}>
                      {p.name}
                    </option>
                  ))}
                </select>
              </label>
            )}
            <label className="block">
              Base
              <input name="base" className={field} defaultValue="main" required />
            </label>
            <label className="block">
              Name
              <input name="name" className={field} required />
            </label>
            <label className="block">
              Model
              <input name="model" className={field} defaultValue="opus" required />
            </label>
            <label className="block">
              Machine
              <select name="machine" className={field} defaultValue="this">
                <option value="this">This machine</option>
              </select>
            </label>
            {create.isError && (
              <p role="alert" className="text-red-400">
                {create.error.message}
              </p>
            )}
            {warning && (
              <p role="status" className="text-amber-400">
                {warning}
              </p>
            )}
            <div className="flex justify-end gap-2">
              <Dialog.Close className={button}>{made ? "Close" : "Cancel"}</Dialog.Close>
              {!made && (
                <button type="submit" className={button} disabled={projects.length === 0 || create.isPending}>
                  Create
                </button>
              )}
            </div>
          </form>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
