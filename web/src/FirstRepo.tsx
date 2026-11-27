import { useMutation } from "@tanstack/react-query";
import { useRef, useState, type FormEvent } from "react";
import { inlineField, primaryButton } from "./Modal";
import { addProject, newId } from "./Projects";

/**
 * The name a repo's address gives its project: the last part of its path, without `.git`. Works
 * for scp-like (`git@host:org/repo.git`), URL and plain path addresses, and leaves out any login
 * or password the address carries. Empty when the address has no name in it.
 */
export function repoName(repo: string): string {
  const last = repo.trim().replace(/\/+$/, "").split(/[/:]/).pop() ?? "";
  return last.replace(/\.git$/i, "");
}

/**
 * The first run's first step when the fleet has no project: the address of the repo Claude will
 * work in, which names the project as it is typed (a typed name wins), and Add. It adds the
 * project as the Projects dialog does, so a retry after an answer that came too late goes under
 * the same id, and a refusal says why and keeps what was typed. `onAdded` gets the project's id
 * once the daemon has taken it; the project shows up in the settings soon after.
 */
export function FirstRepo({ onAdded }: { onAdded: (id: string) => void }) {
  const [repo, setRepo] = useState("");
  const [name, setName] = useState("");
  /** The id the typed project is added under, until the typing changes. */
  const pendingId = useRef<string | null>(null);
  const add = useMutation({ mutationFn: addProject, onSuccess: onAdded });
  const chosen = name.trim() || repoName(repo);
  const sent = add.isPending || add.isSuccess;
  const ready = repo.trim() !== "" && chosen !== "" && !sent;
  const typed = (set: (value: string) => void) => (value: string) => {
    pendingId.current = null;
    set(value);
  };
  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (!ready) return;
    pendingId.current ??= newId();
    add.mutate({ name: chosen, repo: repo.trim(), id: pendingId.current });
  };

  return (
    <form onSubmit={submit} className="space-y-2">
      <div className="rounded-2xl border border-neutral-700 bg-neutral-900 shadow-2xl shadow-black/30 focus-within:border-neutral-400">
        <input
          aria-label="Repo URL"
          autoFocus
          autoComplete="off"
          spellCheck={false}
          disabled={sent}
          value={repo}
          onChange={(e) => typed(setRepo)(e.target.value)}
          placeholder="git@github.com:you/repo.git"
          className="block w-full min-w-0 rounded-t-2xl bg-transparent px-5 pt-4 pb-3 font-mono text-base text-neutral-100 outline-none placeholder:text-neutral-600 disabled:text-neutral-400"
        />
        <div className="flex flex-wrap items-center gap-x-2 gap-y-2 border-t border-neutral-800 px-3 py-2.5 text-xs text-neutral-400">
          <label className="flex min-w-0 items-center gap-1.5">
            named
            <input
              aria-label="Project name"
              autoComplete="off"
              spellCheck={false}
              disabled={sent}
              value={name}
              onChange={(e) => typed(setName)(e.target.value)}
              placeholder={repoName(repo) || "from the repo"}
              className={`${inlineField} w-40 max-w-full font-sans`}
            />
          </label>
          <button type="submit" disabled={!ready} className={`${primaryButton} ml-auto px-3 py-1`}>
            Add
          </button>
        </div>
      </div>
      <p aria-live="polite" className="min-h-4 px-1 text-xs text-neutral-400">
        {sent ? `Adding ${chosen} to the fleet…` : "Each machine clones it the first time it hosts a task for it. Every machine shares the list."}
      </p>
      {add.isError && (
        <p role="alert" className="px-1 text-sm text-red-400">
          {add.error.message}
        </p>
      )}
    </form>
  );
}
