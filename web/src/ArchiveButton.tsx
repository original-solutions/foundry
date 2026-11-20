import { useMutation } from "@tanstack/react-query";
import { useState } from "react";
import { Dialog } from "radix-ui";
import { archiveWorkspace, type Blockers } from "./api";

export const button = "rounded border border-neutral-600 px-2 py-0.5 text-xs hover:bg-neutral-800 disabled:opacity-50";

/** Archives a workspace. When something holds it up, lists it and offers to archive anyway. */
export function ArchiveButton({ workspace, name }: { workspace: string; name: string }) {
  const [blockers, setBlockers] = useState<Blockers | null>(null);
  const archive = useMutation({
    mutationFn: (force: boolean) => archiveWorkspace(workspace, force),
    onSuccess: (held) => setBlockers(held),
  });
  const run = (force: boolean) => archive.mutate(force);

  return (
    <>
      <button type="button" className={button} onClick={() => run(false)} disabled={archive.isPending}>
        Archive
      </button>
      {archive.isError && !blockers && (
        <p role="alert" className="w-full text-xs text-red-400">
          {archive.error.message}
        </p>
      )}
      <Dialog.Root open={blockers !== null} onOpenChange={(open) => !open && setBlockers(null)}>
        <Dialog.Portal>
          <Dialog.Overlay className="fixed inset-0 bg-black/60" />
          <Dialog.Content className="fixed left-1/2 top-1/2 w-96 -translate-x-1/2 -translate-y-1/2 rounded bg-neutral-900 p-4 text-sm text-neutral-100 shadow-xl">
            <Dialog.Title className="font-semibold">Archive {name}?</Dialog.Title>
            <Dialog.Description className="mt-1 text-neutral-400">Archiving now would lose this work.</Dialog.Description>
            {blockers && (
              <div className="mt-2 space-y-2">
                <List title="Uncommitted files" items={blockers.uncommitted} />
                <List title="Unpushed commits" items={blockers.unpushed} />
                <List title="Submodules" items={blockers.submodules} />
              </div>
            )}
            {archive.isError && blockers && (
              <p role="alert" className="mt-2 text-xs text-red-400">
                {archive.error.message}
              </p>
            )}
            <div className="mt-4 flex justify-end gap-2">
              <Dialog.Close className={button}>Cancel</Dialog.Close>
              <button type="button" className={button} onClick={() => run(true)} disabled={archive.isPending}>
                Archive anyway
              </button>
            </div>
          </Dialog.Content>
        </Dialog.Portal>
      </Dialog.Root>
    </>
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
