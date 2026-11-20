import { useMutation } from "@tanstack/react-query";
import { useState, type ReactElement } from "react";
import { Dialog } from "radix-ui";
import { archiveWorkspace, type Blockers } from "./api";
import { button, Modal } from "./Modal";

/**
 * Archives a workspace after asking, since archiving stops its terminals. When something holds
 * it up, lists it and offers to archive anyway. `trigger` replaces the default text button.
 */
export function ArchiveButton({ workspace, name, trigger = "Archive" }: { workspace: string; name: string; trigger?: ReactElement | string }) {
  const [open, setOpen] = useState(false);
  const [blockers, setBlockers] = useState<Blockers | null>(null);
  const archive = useMutation({
    mutationFn: (force: boolean) => archiveWorkspace(workspace, force),
  });
  // Given per call, so a call the dialog was reset from cannot change it.
  const run = (force: boolean) =>
    archive.mutate(force, {
      onSuccess: (held) => {
        setBlockers(held);
        if (!held) setOpen(false);
      },
    });
  const change = (next: boolean) => {
    setOpen(next);
    setBlockers(null);
    archive.reset();
  };

  return (
    <Modal
      open={open}
      onOpenChange={change}
      trigger={trigger}
      title={`Archive ${name}?`}
      description={blockers ? "Archiving now would lose this work." : "Its terminals stop and its worktree is removed. The branch stays."}
    >
      {blockers && (
        <div className="mt-2 space-y-2">
          <List title="Uncommitted files" items={blockers.uncommitted} />
          <List title="Unpushed commits" items={blockers.unpushed} />
          <List title="Submodules" items={blockers.submodules} />
        </div>
      )}
      {archive.isError && (
        <p role="alert" className="mt-2 text-xs text-red-400">
          {archive.error.message}
        </p>
      )}
      <div className="mt-4 flex justify-end gap-2">
        <Dialog.Close className={button}>Cancel</Dialog.Close>
        <button type="button" className={button} onClick={() => run(blockers !== null)} disabled={archive.isPending}>
          {blockers ? "Archive anyway" : "Archive"}
        </button>
      </div>
    </Modal>
  );
}

function List({ title, items }: { title: string; items: string[] }) {
  if (items.length === 0) return null;
  return (
    <div>
      <h3 className="text-xs uppercase text-neutral-400">{title}</h3>
      <ul className="font-mono text-xs">
        {items.map((item) => (
          <li key={item}>{item}</li>
        ))}
      </ul>
    </div>
  );
}
