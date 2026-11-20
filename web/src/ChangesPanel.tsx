import { useQuery } from "@tanstack/react-query";
import { Dialog } from "radix-ui";
import { useRef, useState, type KeyboardEvent } from "react";
import { workspaceChanges, type Changes, type FileChange } from "./api";
import { diffLines, splitPath, totals } from "./changes";
import { freshLines, loadViewed, nextUnviewed, progress, saveViewed, seen, standing, type Seen, type Standing, type Viewed } from "./review";

/** How often an open panel reads the changes again, so an agent's edits show as it makes them. */
export const CHANGES_EVERY = 3000;

/** A file with more diff lines than this starts folded. */
const FOLDED_PAST = 400;

/** The query an open panel keeps fresh. The header reads its cached counts. */
export const changesQuery = (workspace: string) => ({ queryKey: ["changes", workspace], queryFn: () => workspaceChanges(workspace) });

const status: Record<FileChange["status"], { letter: string; title: string; tone: string }> = {
  added: { letter: "A", title: "Added", tone: "bg-emerald-400/15 text-emerald-300" },
  modified: { letter: "M", title: "Modified", tone: "bg-amber-400/15 text-amber-300" },
  deleted: { letter: "D", title: "Deleted", tone: "bg-red-400/15 text-red-300" },
  renamed: { letter: "R", title: "Renamed", tone: "bg-sky-400/15 text-sky-300" },
  untracked: { letter: "N", title: "New, not added to git yet", tone: "bg-violet-400/15 text-violet-300" },
};

interface Target {
  id: string;
  name: string;
  branch: string;
}

/**
 * Everything a workspace changed since it left its base, as a sheet over the right of the screen:
 * the totals, the commits on its branch, a list of files to jump between and each file's diff.
 * It reads again every few seconds while open, so what an agent writes shows up as it writes it.
 * On a narrow screen it takes the whole screen and the file list folds away.
 *
 * It is also a review: tick a file Viewed and it folds away and counts toward "n of m viewed".
 * When an agent changes a viewed file again, the file opens, says it changed since you viewed it
 * and marks the lines that are new since then. j and k step through the files, v ticks the one
 * you are on and moves to the next one still to view. What you viewed is kept in this browser.
 */
export function ChangesPanel({ workspace, onClose }: { workspace?: Target; onClose: () => void }) {
  return (
    <Dialog.Root open={workspace !== undefined} onOpenChange={(open) => !open && onClose()}>
      <Dialog.Portal>{workspace && <Sheet key={workspace.id} workspace={workspace} onClose={onClose} />}</Dialog.Portal>
    </Dialog.Root>
  );
}

function Sheet({ workspace, onClose }: { workspace: Target; onClose: () => void }) {
  const { data, error, isPending } = useQuery({ ...changesQuery(workspace.id), refetchInterval: CHANGES_EVERY, retry: false });
  const body = useRef<HTMLDivElement>(null);
  const [viewed, setViewed] = useState<Viewed>(() => loadViewed(workspace.id));
  const [at, setAt] = useState<number>();
  const files = data?.files ?? [];
  const current = at !== undefined && at < files.length ? at : undefined;

  const show = (i: number) => {
    setAt(i);
    body.current?.querySelector(`[data-path="${CSS.escape(files[i].path)}"]`)?.scrollIntoView?.({ block: "start" });
  };
  const mark = (file: FileChange, on: boolean) => {
    const next = { ...viewed };
    if (on) next[file.path] = seen(file);
    else delete next[file.path];
    setViewed(next);
    saveViewed(workspace.id, next, files);
    return next;
  };
  const keys = (e: KeyboardEvent) => {
    if (e.metaKey || e.ctrlKey || e.altKey || files.length === 0) return;
    if (e.key === "j") show(current === undefined ? 0 : Math.min(current + 1, files.length - 1));
    else if (e.key === "k") show(current === undefined ? 0 : Math.max(current - 1, 0));
    else if (e.key === "v") {
      const i = current ?? nextUnviewed(files, viewed, files.length - 1) ?? 0;
      const on = standing(files[i], viewed[files[i].path]) !== "viewed";
      const next = mark(files[i], on);
      show(on ? (nextUnviewed(files, next, i) ?? i) : i);
    } else return;
    e.preventDefault();
  };

  return (
    <>
      <Dialog.Overlay className="fixed inset-0 z-40 bg-black/50" />
      <Dialog.Content
        onKeyDown={keys}
        className="fixed inset-y-0 right-0 z-50 flex w-full flex-col border-l border-neutral-800 bg-neutral-950 text-sm text-neutral-100 shadow-2xl shadow-black outline-none md:w-[min(72rem,calc(100vw-5rem))] motion-safe:animate-sheet-in"
      >
        <header className="flex shrink-0 items-start gap-3 border-b border-neutral-800 bg-neutral-900/80 px-4 py-3">
          <div className="min-w-0 flex-1">
            <Dialog.Title className="flex items-baseline gap-2 text-base font-semibold">
              <span>Changes</span>{" "}
              <span className="sr-only">in</span>{" "}
              <span className="truncate text-neutral-400">{workspace.name}</span>
            </Dialog.Title>
            <Dialog.Description asChild>
              <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-neutral-400">
                <span className="font-mono">
                  <span className="text-neutral-200">{workspace.branch}</span>
                  <span className="px-1.5 text-neutral-600">←</span>
                  {data?.base ?? "…"}
                </span>
                {data && data.files.length > 0 && <Summary changes={data} />}
                {files.length > 0 && <Progress {...progress(files, viewed)} />}
                <span className="text-neutral-600">Updates live</span>
                {files.length > 0 && (
                  <span className="hidden text-neutral-600 md:inline">
                    <Key>j</Key> <Key>k</Key> files · <Key>v</Key> viewed
                  </span>
                )}
              </div>
            </Dialog.Description>
          </div>
          <Dialog.Close aria-label="Close changes" className="-mr-1 rounded p-1.5 text-neutral-400 hover:bg-neutral-800 hover:text-neutral-100">
            <svg viewBox="0 0 16 16" className="size-4" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
              <path d="M4 4l8 8M12 4l-8 8" strokeLinecap="round" />
            </svg>
          </Dialog.Close>
        </header>
        {error ? (
          <div className="p-4">
            <p role="alert" className="rounded border border-red-500/30 bg-red-500/10 px-3 py-2 text-red-300">
              {error.message}
            </p>
          </div>
        ) : isPending ? (
          <p className="p-4 text-neutral-500">Reading changes…</p>
        ) : data.files.length === 0 && data.commits.length === 0 ? (
          <Empty base={data.base} onClose={onClose} />
        ) : (
          <div className="flex min-h-0 flex-1">
            <nav aria-label="Changed files" className="hidden w-64 shrink-0 overflow-y-auto border-r border-neutral-800 py-2 md:block">
              {data.files.map((f, i) => (
                <FileLink key={f.path} file={f} state={standing(f, viewed[f.path])} current={i === current} onPick={() => show(i)} />
              ))}
            </nav>
            <div ref={body} className="min-w-0 flex-1 space-y-3 overflow-y-auto p-3 md:p-4">
              {data.commits.length > 0 && <Commits commits={data.commits} branch={workspace.branch} />}
              {data.files.map((f, i) => {
                const state = standing(f, viewed[f.path]);
                return (
                  <FileDiff
                    key={`${f.path}:${state}`}
                    file={f}
                    state={state}
                    kept={viewed[f.path]}
                    current={i === current}
                    onViewed={(on) => {
                      mark(f, on);
                      setAt(i);
                    }}
                  />
                );
              })}
            </div>
          </div>
        )}
      </Dialog.Content>
    </>
  );
}

function Key({ children }: { children: string }) {
  return <kbd className="rounded border border-neutral-700 px-1 font-mono text-[10px] text-neutral-400">{children}</kbd>;
}

/** How far through reviewing the change you are, as a bar and words; amber when files changed since you viewed them. */
function Progress({ total, viewed, changed }: { total: number; viewed: number; changed: number }) {
  const all = viewed === total;
  return (
    <span className="flex items-center gap-2">
      <span aria-hidden="true" className="h-1 w-14 overflow-hidden rounded-full bg-neutral-800">
        <span className={`block h-full rounded-full motion-safe:transition-[width] ${all ? "bg-emerald-400" : "bg-sky-400"}`} style={{ width: `${(viewed / total) * 100}%` }} />
      </span>
      <span className={all ? "text-emerald-300" : "text-neutral-300"}>{all ? `All ${total} viewed` : `${viewed} of ${total} viewed`}</span>
      {changed > 0 && <span className="text-amber-300">{changed} changed since viewed</span>}
    </span>
  );
}

function Summary({ changes }: { changes: Changes }) {
  const t = totals(changes);
  const plural = (n: number, one: string) => `${n} ${one}${n === 1 ? "" : "s"}`;
  return (
    <>
      <span className="font-mono">
        <span className="text-emerald-400">+{t.added}</span> <span className={t.removed ? "text-red-400" : "text-neutral-600"}>−{t.removed}</span>
      </span>
      <span>{plural(t.files, "file")}</span>
      {t.uncommitted > 0 && <span className="text-amber-300">{t.uncommitted} uncommitted</span>}
      {changes.commits.length > 0 && <span>{plural(changes.commits.length, "commit")}</span>}
    </>
  );
}

function Empty({ base, onClose }: { base: string; onClose: () => void }) {
  return (
    <div className="flex flex-1 flex-col items-center justify-center gap-2 px-6 pb-[10vh] text-center">
      <p className="text-base text-neutral-300">No changes since {base} yet.</p>
      <p className="max-w-sm text-xs text-neutral-500">What the agents and shells write in this workspace shows up here as they write it, committed or not.</p>
      <button type="button" onClick={onClose} className="mt-2 text-xs text-neutral-400 underline decoration-neutral-700 underline-offset-4 hover:text-neutral-200">
        Back to the terminal
      </button>
    </div>
  );
}

function StatusChip({ file }: { file: FileChange }) {
  const s = status[file.status];
  return (
    <span title={s.title} className={`inline-flex size-4 shrink-0 items-center justify-center rounded-sm font-mono text-[10px] font-semibold ${s.tone}`}>
      {s.letter}
    </span>
  );
}

/** Five blocks split between added and removed lines, as a glance at the size and kind of change. */
function Bar({ file }: { file: FileChange }) {
  const added = file.added ?? 0;
  const removed = file.removed ?? 0;
  const all = added + removed;
  const green = all === 0 ? 0 : Math.round((added / all) * 5);
  const red = all === 0 ? 0 : Math.min(5 - green, Math.max(removed > 0 ? 1 : 0, Math.round((removed / all) * 5)));
  return (
    <span aria-hidden="true" className="flex gap-px">
      {Array.from({ length: 5 }, (_, i) => (
        <span key={i} className={`size-1.5 rounded-[1px] ${i < green ? "bg-emerald-400" : i < green + red ? "bg-red-400" : "bg-neutral-700"}`} />
      ))}
    </span>
  );
}

function Counts({ file }: { file: FileChange }) {
  if (file.added === null) return <span className="text-neutral-500">binary</span>;
  return (
    <span className="whitespace-nowrap font-mono">
      <span className="text-emerald-400">+{file.added}</span> <span className={file.removed ? "text-red-400" : "text-neutral-600"}>−{file.removed}</span>
    </span>
  );
}

function FileLink({ file, state, current, onPick }: { file: FileChange; state: Standing; current: boolean; onPick: () => void }) {
  const { dir, name } = splitPath(file.path);
  return (
    <button
      type="button"
      onClick={onPick}
      title={file.path}
      className={`flex w-full items-center gap-2 border-l-2 px-3 py-1 text-left text-xs hover:bg-neutral-900 ${current ? "border-sky-400 bg-neutral-900" : "border-transparent"}`}
    >
      <StatusChip file={file} />
      <span className="min-w-0 flex-1 truncate">
        <span className={state === "viewed" ? "text-neutral-500" : "text-neutral-200"}>{name}</span>
        {dir && <span className="ml-1.5 text-neutral-600">{dir.slice(0, -1)}</span>}
      </span>
      {state === "viewed" ? <Tick /> : state === "changed" ? <span title="Changed since viewed" className="size-1.5 rounded-full bg-amber-300" /> : <Bar file={file} />}
    </button>
  );
}

function Tick() {
  return (
    <svg viewBox="0 0 16 16" className="size-3.5 text-emerald-400" fill="none" stroke="currentColor" strokeWidth="2" aria-label="Viewed">
      <path d="M3.5 8.5l3 3 6-7" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}

function Commits({ commits, branch }: { commits: string[]; branch: string }) {
  return (
    <section aria-label="Commits" className="rounded-lg border border-neutral-800 bg-neutral-900/60 px-3 py-2">
      <h3 className="mb-1 text-xs text-neutral-500">
        {commits.length} commit{commits.length === 1 ? "" : "s"} on <span className="font-mono text-neutral-300">{branch}</span>
      </h3>
      <ul className="space-y-0.5 text-xs">
        {commits.map((c) => {
          const at = c.indexOf(" ");
          return (
            <li key={c} className="flex gap-2">
              <span className="shrink-0 font-mono text-amber-300/80">{c.slice(0, at)}</span>
              <span className="truncate text-neutral-300">{c.slice(at + 1)}</span>
            </li>
          );
        })}
      </ul>
    </section>
  );
}

const rowTone = {
  added: "bg-emerald-500/10 text-emerald-100",
  removed: "bg-red-500/10 text-red-100",
  same: "text-neutral-300",
};
const marker = { added: "+", removed: "-", same: " " };

interface FileDiffProps {
  file: FileChange;
  state: Standing;
  /** The file as it was when viewed, to mark the lines new since then. */
  kept: Seen | undefined;
  /** The file j, k and v act on. */
  current: boolean;
  onViewed: (on: boolean) => void;
}

/** One file's diff. The parent remounts it when its standing changes, so ticking Viewed folds it and a change since opens it. */
function FileDiff({ file, state, kept, current, onViewed }: FileDiffProps) {
  const lines = file.patch === null ? [] : diffLines(file.patch);
  const [open, setOpen] = useState(state !== "viewed" && lines.length <= FOLDED_PAST);
  const fresh = state === "changed" ? freshLines(lines, kept) : new Set<number>();
  const { dir, name } = splitPath(file.path);
  return (
    <section
      aria-label={file.path}
      aria-current={current || undefined}
      data-path={file.path}
      className={`scroll-mt-3 overflow-hidden rounded-lg border bg-neutral-900/40 ${current ? "border-sky-400/70" : state === "changed" ? "border-amber-400/40" : "border-neutral-800"}`}
    >
      <h3 className="sticky top-0 z-10 flex items-stretch border-b border-neutral-800 bg-neutral-900">
        <button type="button" aria-expanded={open} onClick={() => setOpen(!open)} className="flex min-w-0 flex-1 items-center gap-2 px-3 py-2 text-left text-xs hover:bg-neutral-800/60">
          <svg viewBox="0 0 16 16" className={`size-3 shrink-0 text-neutral-500 transition-transform ${open ? "rotate-90" : ""}`} fill="currentColor" aria-hidden="true">
            <path d="M6 4l4 4-4 4z" />
          </svg>
          <StatusChip file={file} />
          <span className="min-w-16 flex-1 truncate font-mono">
            <span className="text-neutral-500">{dir}</span>
            <span className={state === "viewed" ? "text-neutral-400" : "text-neutral-100"}>{name}</span>
            {file.from && <span className="ml-2 text-neutral-500">from {file.from}</span>}
          </span>
          {state === "changed" && (
            <span className="shrink-0 rounded bg-amber-400/10 px-1.5 py-px text-amber-300">
              Changed<span className="max-sm:hidden"> since viewed</span>
            </span>
          )}
          {file.status === "untracked" && <span className="hidden rounded bg-violet-400/10 px-1.5 py-px text-violet-300 sm:inline">New</span>}
          {file.uncommitted && <span className="rounded bg-amber-400/10 px-1.5 py-px text-amber-300 max-sm:hidden">Uncommitted</span>}
          <Counts file={file} />
          <span className="hidden sm:flex">
            <Bar file={file} />
          </span>
        </button>
        <label className="flex shrink-0 cursor-pointer items-center gap-1.5 border-l border-neutral-800 px-3 text-xs text-neutral-400 hover:bg-neutral-800/60 hover:text-neutral-100">
          <input type="checkbox" checked={state === "viewed"} onChange={(e) => onViewed(e.target.checked)} className="size-3.5 cursor-pointer accent-emerald-400" />
          Viewed
        </label>
      </h3>
      {open &&
        (file.patch === null ? (
          <p className="px-3 py-3 text-xs text-neutral-500">
            {file.added === null ? "Binary, or not a plain file: not shown." : "Too large to show here."}
          </p>
        ) : (
          <div role="table" aria-label={`Diff of ${file.path}`} className="overflow-x-auto py-1 font-mono text-xs leading-5">
            {lines.map((line, i) =>
              line.kind === "hunk" ? (
                <div key={i} className="whitespace-pre bg-sky-500/5 px-3 text-sky-300/70">
                  {line.text}
                </div>
              ) : line.kind === "note" ? (
                <div key={i} className="px-3 text-neutral-500 italic">
                  {line.text}
                </div>
              ) : (
                <div key={i} role="row" aria-label={`${line.kind} line${fresh.has(i) ? ", new since viewed" : ""}`} className={`flex min-w-max ${rowTone[line.kind]}`}>
                  <span role="cell" className={`w-10 shrink-0 select-none border-l-[3px] pr-2 text-right text-neutral-600 ${fresh.has(i) ? "border-sky-400" : "border-transparent"}`}>
                    {line.kind === "added" ? "" : line.old}
                  </span>
                  <span role="cell" className="w-10 shrink-0 select-none pr-2 text-right text-neutral-600">
                    {line.kind === "removed" ? "" : line.new}
                  </span>
                  <span role="cell" className="w-4 shrink-0 select-none text-center opacity-70">
                    {marker[line.kind]}
                  </span>
                  <span role="cell" className="whitespace-pre pr-4">
                    {line.text}
                  </span>
                </div>
              ),
            )}
          </div>
        ))}
    </section>
  );
}
