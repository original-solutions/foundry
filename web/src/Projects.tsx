import { useState, type FormEvent } from "react";
import { post, refused, type Project } from "./api";
import { button, field, Modal, primaryButton } from "./Modal";

/** Adds a project through the daemon, which sends it to the acting coordinator; its new id. */
async function addProject(name: string, repo: string): Promise<string> {
  const { status, json } = await post("/api/projects", { name, repo });
  if (status !== 201) throw refused(status, json);
  return String(json.id);
}

/** Removes a project; the daemon refuses while a live workspace was made from it. */
async function removeProject(id: string): Promise<void> {
  const { status, json } = await post(`/api/projects/${id}/remove`);
  if (status !== 204) throw refused(status, json);
}

const said = (error: unknown) => (error instanceof Error ? error.message : String(error));

/**
 * The fleet's projects, each with its repo as the settings show it (any password hidden) and a
 * Remove button, and a form that adds one. A change shows once it arrives in the settings, on
 * every machine. A refused add keeps what was typed and says why under the form; a refused
 * remove says why beside its project. `onAdded` is given each new project's id.
 */
export function Projects({
  open,
  onOpenChange,
  projects,
  onAdded,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  projects: Project[];
  onAdded: (id: string) => void;
}) {
  const [name, setName] = useState("");
  const [repo, setRepo] = useState("");
  const [adding, setAdding] = useState(false);
  const [addError, setAddError] = useState<string | null>(null);
  const [removing, setRemoving] = useState<string | null>(null);
  const [removeErrors, setRemoveErrors] = useState<Record<string, string>>({});
  const ready = name.trim() !== "" && repo.trim() !== "" && !adding;

  const add = async (event: FormEvent) => {
    event.preventDefault();
    if (!ready) return;
    setAdding(true);
    setAddError(null);
    try {
      const id = await addProject(name.trim(), repo.trim());
      setName("");
      setRepo("");
      onAdded(id);
    } catch (error) {
      setAddError(said(error));
    } finally {
      setAdding(false);
    }
  };

  const remove = async (id: string) => {
    setRemoving(id);
    setRemoveErrors((errors) => Object.fromEntries(Object.entries(errors).filter(([held]) => held !== id)));
    try {
      await removeProject(id);
    } catch (error) {
      setRemoveErrors((errors) => ({ ...errors, [id]: said(error) }));
    } finally {
      setRemoving(null);
    }
  };

  return (
    <Modal
      open={open}
      onOpenChange={onOpenChange}
      wide
      title="Projects"
      description="A project is a repo and a name, shared by every machine. A machine clones it the first time it hosts a workspace for it."
    >
      {projects.length === 0 ? (
        <p className="mt-4 text-neutral-500">No projects yet.</p>
      ) : (
        <ul className="mt-4 divide-y divide-neutral-800 rounded-lg border border-neutral-800">
          {projects.map((p) => (
            <li key={p.id} aria-label={p.name} className="px-3 py-2">
              <div className="flex items-center gap-3">
                <div className="min-w-0 flex-1">
                  <p className="truncate font-medium text-neutral-100">{p.name}</p>
                  <p className="truncate font-mono text-xs text-neutral-500" title={p.repo}>
                    {p.repo}
                  </p>
                </div>
                <button type="button" onClick={() => remove(p.id)} disabled={removing !== null} aria-label={`Remove ${p.name}`} className={button}>
                  Remove
                </button>
              </div>
              {removeErrors[p.id] && (
                <p role="alert" className="mt-1 text-xs text-red-400">
                  {removeErrors[p.id]}
                </p>
              )}
            </li>
          ))}
        </ul>
      )}

      <form onSubmit={add} className="mt-5 space-y-3">
        <label className="block text-xs text-neutral-400">
          Name
          <input name="name" autoComplete="off" spellCheck={false} value={name} onChange={(e) => setName(e.target.value)} placeholder="foundry" className={field} />
        </label>
        <label className="block text-xs text-neutral-400">
          Repo URL
          <input
            name="repo"
            autoComplete="off"
            spellCheck={false}
            value={repo}
            onChange={(e) => setRepo(e.target.value)}
            placeholder="git@github.com:org/repo.git"
            className={`${field} font-mono`}
          />
        </label>
        {addError && (
          <p role="alert" className="text-xs text-red-400">
            {addError}
          </p>
        )}
        <div className="flex justify-end gap-2">
          <button type="button" onClick={() => onOpenChange(false)} className={button}>
            Close
          </button>
          <button type="submit" disabled={!ready} className={primaryButton}>
            Add
          </button>
        </div>
      </form>
    </Modal>
  );
}
