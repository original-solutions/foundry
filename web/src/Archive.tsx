import { useMutation, useMutationState, useQuery } from "@tanstack/react-query";
import { useId, useState, type ReactNode } from "react";
import { Dialog } from "radix-ui";
import { ApiError, archiveWorkspace, type Blockers, type FileChange } from "./api";
import { changesQuery } from "./ChangesPanel";
import { splitPath } from "./changes";
import { rows, type MachineView, type WorkspaceView } from "./fleet";
import { couldNot, plural, words, type Glyph } from "./labels";
import { button, dangerButton, Modal, primaryButton } from "./Modal";
import { Named } from "./Now";
import { glyphText, StateGlyph } from "./StateGlyph";

/** The commits shown before the older ones are counted. */
const SHOWN = 5;

/** What stopping a terminal in this state cuts short. */
const cut: Partial<Record<Glyph, string>> = {
  working: "stops mid-turn",
  starting: "stops before it starts",
  waiting: "stops without your answer",
};

/** Archiving `workspace`: one mutation per workspace, so every view can see it under way or done. */
export const archiveOf = (workspace: string) => ({
  mutationKey: ["archive", workspace],
  mutationFn: (force: boolean) => archiveWorkspace(workspace, force),
});

/**
 * Whether `workspace` is being archived from this page, or was. Its worktree is going or gone,
 * so its changes are not read or shown: a read mid-archive would see its files as deleted.
 */
export function useLeaving(workspace: string): boolean {
  const leaving = useMutationState({
    filters: { mutationKey: archiveOf(workspace).mutationKey, predicate: (m) => m.state.status === "pending" || (m.state.status === "success" && m.state.data === null) },
  });
  return leaving.length > 0;
}

/**
 * Asks before archiving `workspace`, and shows what archiving it means before anything is sent:
 * the commits that stay on its branch, the uncommitted files that go with the worktree, and every
 * terminal that stops, with a turn it would cut short. Its changes are read once, as the changes
 * sheet reads them, or the daemon's reason they cannot be; the daemon still decides, and when it
 * holds the archive up, its list of what would be lost is shown, with a folder that is no longer a
 * git worktree said so, and Archive anyway forces it. While the archive runs, its button says so.
 *
 * For a workspace on another machine than `here`, the one serving the UI, its changes are not read:
 * that read answers only for this machine's workspaces. The dialog says so, still lists the
 * terminals that stop, and relies on the owning machine, which the daemon carries the archive to,
 * to refuse when work would be lost. While `here` is not known, the changes are read as for this machine.
 */
export function ArchiveDialog({
  workspace,
  machine,
  here,
  onClose,
  onArchived,
}: {
  workspace: WorkspaceView;
  /** The machine that owns the workspace. */
  machine: Pick<MachineView, "id" | "name">;
  /** The machine serving the UI, once known. */
  here?: string;
  onClose: () => void;
  onArchived: (name: string, branch: string) => void;
}) {
  const [blockers, setBlockers] = useState<Blockers | null>(null);
  const leaving = useLeaving(workspace.id);
  const elsewhere = here !== undefined && machine.id !== here;
  const changes = useQuery({ ...changesQuery(workspace.id), enabled: !leaving && !elsewhere, retry: false });
  const archive = useMutation({
    ...archiveOf(workspace.id),
    onSuccess: (held) => {
      setBlockers(held);
      if (held) return;
      onArchived(workspace.name, workspace.branch);
      onClose();
    },
  });
  const lost = changes.data?.files.filter((f) => f.uncommitted) ?? [];
  const terminals = rows(workspace);
  const cuts = terminals.some((r) => cut[r.glyph]);
  const risky = lost.length > 0 || cuts || blockers !== null;

  return (
    <Modal
      open
      wide
      onOpenChange={(open) => !open && onClose()}
      title={`Archive ${workspace.name}?`}
      description={
        <>
          Its terminals stop and its worktree is removed. Branch <Branch name={workspace.branch} /> stays.
        </>
      }
    >
      <div className="mt-4 flex max-h-[min(60dvh,32rem)] flex-col gap-4 overflow-y-auto">
        <Part title={`Kept on ${workspace.branch}`} tone="text-emerald-400" icon="M4 2.5v11M4 5.5c0 2 1.5 3 4 3h1.5M11.5 8.5a1.5 1.5 0 1 0 3 0 1.5 1.5 0 0 0-3 0">
          {elsewhere ? (
            <p className="text-xs text-neutral-500">
              {words.unreadable}. {machine.name} lists any work that would be lost before it archives.
            </p>
          ) : changes.isError ? (
            // The daemon says why it could not read them, as for a folder that is no longer a git worktree.
            // #323: any refusal's message (a 500's too) shows in place of words.unreadable.
            <p className="text-xs text-neutral-500">{changes.error instanceof ApiError ? changes.error.message.trim().replace(/\.$/, "") : words.unreadable}.</p>
          ) : !changes.data ? (
            <p className="text-xs text-neutral-600">Reading what it changed…</p>
          ) : changes.data.commits.length === 0 ? (
            <p className="text-xs text-neutral-500">No commits since {changes.data.base}: only the branch name stays.</p>
          ) : (
            <ul className="flex flex-col gap-0.5 text-xs">
              {changes.data.commits.slice(0, SHOWN).map((c) => (
                <li key={c.hash} className="flex min-w-0 items-baseline gap-2">
                  <span className="shrink-0 font-mono text-[11px] text-neutral-500">{c.hash}</span>
                  <span className="truncate text-neutral-200">{c.subject}</span>
                </li>
              ))}
              {changes.data.commits.length > SHOWN && <li className="text-neutral-500">and {plural(changes.data.commits.length - SHOWN, "older commit")}</li>}
            </ul>
          )}
          {changes.data && lost.length === 0 && changes.data.files.length > 0 && <p className="mt-1.5 text-xs text-emerald-400">Every change is committed.</p>}
        </Part>

        {lost.length > 0 && (
          <Part title="Lost with the worktree" tone="text-amber-300" icon="M8 2.5l6 11H2zM8 6.5v3M8 11.5v.01" warn>
            <p className="mb-1 text-xs text-neutral-400">{plural(lost.length, "file")} with work not committed:</p>
            <ul className="flex flex-col gap-0.5 font-mono text-xs">
              {lost.map((f) => (
                <Lost key={f.path} file={f} />
              ))}
            </ul>
          </Part>
        )}

        <Part title="Stops" tone="text-neutral-300" icon="M8 2.5v5M4.8 4.6a4.8 4.8 0 1 0 6.4 0">
          {terminals.length === 0 ? (
            <p className="text-xs text-neutral-500">No terminals.</p>
          ) : (
            <ul className="flex flex-col gap-0.5 text-sm">
              {terminals.map((r) => (
                <li key={r.terminal} className={`flex min-w-0 items-center gap-2.5 ${r.glyph === "done" || r.glyph === "away" ? "text-neutral-500" : "text-neutral-200"}`}>
                  <StateGlyph glyph={r.glyph} />
                  <Named name={r.name} title={r.title} />
                  <span className={`shrink-0 text-xs ${glyphText[r.glyph]}`}>{r.label}</span>
                  {cut[r.glyph] && <span className="shrink-0 rounded bg-amber-400/15 px-1.5 text-[11px] text-amber-300">{cut[r.glyph]}</span>}
                </li>
              ))}
            </ul>
          )}
        </Part>

        {blockers && (
          <Part title="Archiving now would lose this work" tone="text-red-400" icon="M8 2.5l6 11H2zM8 6.5v3M8 11.5v.01" warn>
            {blockers.detached && (
              <p className="text-xs text-neutral-300">
                Its folder is no longer a git worktree, so what is in it can't be checked. Archive anyway deletes the folder; branch <Branch name={workspace.branch} /> stays.
              </p>
            )}
            <Held title="Uncommitted files" items={blockers.uncommitted} />
            <Held title="Unpushed commits" items={blockers.unpushed} />
            <Held title="Submodules" items={blockers.submodules} />
          </Part>
        )}
      </div>
      {archive.isError && (
        <p role="alert" className="mt-3 text-xs text-red-400">
          {couldNot(`archive ${workspace.name}`, archive.error)}
        </p>
      )}
      <div className="mt-5 flex justify-end gap-2">
        <Dialog.Close className={button}>Cancel</Dialog.Close>
        <button
          type="button"
          onClick={() => archive.mutate(blockers !== null)}
          disabled={archive.isPending}
          className={risky ? dangerButton : primaryButton}
        >
          {archive.isPending ? "Archiving…" : blockers ? "Archive anyway" : "Archive"}
        </button>
      </div>
    </Modal>
  );
}

/** A branch's name, as code. */
function Branch({ name }: { name: string }) {
  return <code className="rounded bg-neutral-800 px-1 font-mono text-xs text-neutral-200">{name}</code>;
}

/** One part of the dialog: a small heading with its icon, then what it says. */
function Part({ title, tone, icon, warn, children }: { title: string; tone: string; icon: string; warn?: boolean; children: ReactNode }) {
  const id = useId();
  return (
    <section aria-labelledby={id} className={warn ? "rounded-lg border border-amber-400/30 bg-amber-400/5 px-3 py-2.5" : ""}>
      <h3 id={id} className={`mb-1.5 flex items-center gap-1.5 text-[11px] font-semibold tracking-wider uppercase ${tone}`}>
        <svg viewBox="0 0 16 16" className="size-3.5 shrink-0" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
          <path d={icon} />
        </svg>
        {title}
      </h3>
      {children}
    </section>
  );
}

function Lost({ file }: { file: FileChange }) {
  const { dir, name } = splitPath(file.path);
  return (
    <li className="flex min-w-0 items-baseline gap-2" title={file.path}>
      <span className="min-w-0 flex-1 truncate">
        <span className="text-neutral-500">{dir}</span>
        <span className="text-neutral-200">{name}</span>
      </span>
      {file.status === "untracked" ? (
        <span className="shrink-0 text-[11px] text-amber-300">new</span>
      ) : (
        file.added !== null && (
          <span className="shrink-0 text-[11px]">
            <span className="text-emerald-400">+{file.added}</span> <span className="text-red-400">−{file.removed}</span>
          </span>
        )
      )}
    </li>
  );
}

function Held({ title, items }: { title: string; items: string[] }) {
  if (items.length === 0) return null;
  return (
    <div className="mt-1">
      <h4 className="text-xs text-neutral-400">{title}</h4>
      <ul className="font-mono text-xs text-neutral-200">
        {items.map((item) => (
          <li key={item}>{item}</li>
        ))}
      </ul>
    </div>
  );
}
