import { useMutation, useQuery } from "@tanstack/react-query";
import { useId, useState, type ReactNode } from "react";
import { Dialog } from "radix-ui";
import { archiveWorkspace, type Blockers, type FileChange } from "./api";
import { changesQuery } from "./ChangesPanel";
import { splitPath } from "./changes";
import { rows, type WorkspaceView } from "./fleet";
import { plural, type Glyph } from "./labels";
import { button, Modal } from "./Modal";
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

/**
 * Asks before archiving `workspace`, and shows what archiving it means before anything is sent:
 * the commits that stay on its branch, the uncommitted files that go with the worktree, and every
 * terminal that stops, with a turn it would cut short. Its changes are read once, as the changes
 * sheet reads them; the daemon still decides, and when it holds the archive up, its list of what
 * would be lost is shown and Archive anyway forces it.
 */
export function ArchiveDialog({ workspace, onClose, onArchived }: { workspace: WorkspaceView; onClose: () => void; onArchived: (name: string, branch: string) => void }) {
  const [blockers, setBlockers] = useState<Blockers | null>(null);
  const changes = useQuery({ ...changesQuery(workspace.id), retry: false });
  const archive = useMutation({
    mutationFn: (force: boolean) => archiveWorkspace(workspace.id, force),
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
          Its terminals stop and its worktree is removed. Branch <code className="rounded bg-neutral-800 px-1 font-mono text-xs text-neutral-200">{workspace.branch}</code> stays.
        </>
      }
    >
      <div className="mt-4 flex max-h-[min(60dvh,32rem)] flex-col gap-4 overflow-y-auto">
        <Part title={`Kept on ${workspace.branch}`} tone="text-emerald-400" icon="M4 2.5v11M4 5.5c0 2 1.5 3 4 3h1.5M11.5 8.5a1.5 1.5 0 1 0 3 0 1.5 1.5 0 0 0-3 0">
          {changes.isError ? (
            <p className="text-xs text-neutral-500">What it changed could not be read from here.</p>
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
            <Held title="Uncommitted files" items={blockers.uncommitted} />
            <Held title="Unpushed commits" items={blockers.unpushed} />
            <Held title="Submodules" items={blockers.submodules} />
          </Part>
        )}
      </div>
      {archive.isError && (
        <p role="alert" className="mt-3 text-xs text-red-400">
          {archive.error.message}
        </p>
      )}
      <div className="mt-5 flex justify-end gap-2">
        <Dialog.Close className={button}>Cancel</Dialog.Close>
        <button
          type="button"
          onClick={() => archive.mutate(blockers !== null)}
          disabled={archive.isPending}
          className={
            risky
              ? "rounded bg-red-400 px-3 py-0.5 text-xs font-semibold text-neutral-950 hover:bg-red-300 disabled:opacity-50"
              : "rounded bg-neutral-100 px-3 py-0.5 text-xs font-semibold text-neutral-950 hover:bg-neutral-300 disabled:opacity-50"
          }
        >
          {blockers ? "Archive anyway" : "Archive"}
        </button>
      </div>
    </Modal>
  );
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
