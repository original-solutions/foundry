import { useMutation } from "@tanstack/react-query";
import { useRef, useState, type FormEvent } from "react";
import { post, refused, type Project } from "./api";
import { useProjectFactory } from "./factory";
import { ProjectFactoryControls } from "./FactorySettings";
import { button, dangerButton, field, Modal, primaryButton } from "./Modal";

/** A project with no factory setting: off. */
const FACTORY_OFF = { on: false, login: "", paused: false };

/**
 * Adds a project through the daemon, which sends it to the acting coordinator, under `id`; its
 * id as the daemon answers it.
 */
async function addProject({ name, repo, id }: { name: string; repo: string; id: string }): Promise<string> {
  const { status, json } = await post("/api/projects", { name, repo, id });
  if (status !== 201) throw refused(status, json);
  return String(json.id);
}

/** Removes a project; the daemon refuses while a live workspace it knows of was made from it. */
async function removeProject(id: string): Promise<void> {
  const { status, json } = await post(`/api/projects/${id}/remove`);
  if (status !== 204) throw refused(status, json);
}

/**
 * A new random v4 UUID. Made from `crypto.getRandomValues`, since `crypto.randomUUID` exists only
 * in a secure context, and the UI opened over a machine's plain-http Tailscale address is not one.
 */
function newId(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

/**
 * The fleet's projects, each with its repo as the settings show it (any password hidden) and a
 * Remove button that asks once more before it sends, and a form that adds one. A change shows
 * once it arrives in the settings, on every machine. A refused add keeps what was typed and says
 * why under the form; a refused remove says why beside its project. An add sent again with
 * nothing retyped goes under the same id, so an add whose answer came too late is not made twice.
 * Each project also has its factory controls: a switch, the GitHub login it acts as and a pause,
 * as the settings last sent them (read from the factory context, so opening the dialog fetches
 * nothing); a refused change says why beside the row. Closing the dialog clears what was typed and
 * every refusal. `onAdded` is given each new project's id.
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
  const factories = useProjectFactory();
  const [name, setName] = useState("");
  const [repo, setRepo] = useState("");
  /** The id the typed project is added under, until it is added or the typing changes. */
  const pendingId = useRef<string | null>(null);
  /** The project whose Remove was clicked once and waits to be confirmed. */
  const [confirming, setConfirming] = useState<string | null>(null);

  const add = useMutation({
    mutationFn: addProject,
    onSuccess: (id) => {
      setName("");
      setRepo("");
      pendingId.current = null;
      onAdded(id);
    },
  });
  const remove = useMutation({ mutationFn: removeProject, onSettled: () => setConfirming(null) });
  const ready = name.trim() !== "" && repo.trim() !== "" && !add.isPending;

  const typed = (set: (value: string) => void) => (value: string) => {
    pendingId.current = null;
    set(value);
  };

  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (!ready) return;
    pendingId.current ??= newId();
    add.mutate({ name: name.trim(), repo: repo.trim(), id: pendingId.current });
  };

  const change = (next: boolean) => {
    if (!next) {
      setName("");
      setRepo("");
      pendingId.current = null;
      setConfirming(null);
      add.reset();
      remove.reset();
    }
    onOpenChange(next);
  };

  return (
    <Modal
      open={open}
      onOpenChange={change}
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
                {confirming === p.id ? (
                  <>
                    <button type="button" onClick={() => setConfirming(null)} disabled={remove.isPending} className={button}>
                      Cancel
                    </button>
                    <button type="button" onClick={() => remove.mutate(p.id)} disabled={remove.isPending} className={dangerButton}>
                      Remove from every machine
                    </button>
                  </>
                ) : (
                  <button
                    type="button"
                    onClick={() => {
                      remove.reset();
                      setConfirming(p.id);
                    }}
                    disabled={remove.isPending}
                    aria-label={`Remove ${p.name}`}
                    className={button}
                  >
                    Remove
                  </button>
                )}
              </div>
              <ProjectFactoryControls project={p} factory={factories?.[p.id] ?? FACTORY_OFF} />
              {remove.isError && remove.variables === p.id && (
                <p role="alert" className="mt-1 text-xs text-red-400">
                  {remove.error.message}
                </p>
              )}
            </li>
          ))}
        </ul>
      )}

      <form onSubmit={submit} className="mt-5 space-y-3">
        <label className="block text-xs text-neutral-400">
          Name
          <input name="name" autoComplete="off" spellCheck={false} value={name} onChange={(e) => typed(setName)(e.target.value)} placeholder="foundry" className={field} />
        </label>
        <label className="block text-xs text-neutral-400">
          Repo URL
          <input
            name="repo"
            autoComplete="off"
            spellCheck={false}
            value={repo}
            onChange={(e) => typed(setRepo)(e.target.value)}
            placeholder="git@github.com:org/repo.git"
            className={`${field} font-mono`}
          />
        </label>
        {add.isError && (
          <p role="alert" className="text-xs text-red-400">
            {add.error.message}
          </p>
        )}
        <div className="flex justify-end gap-2">
          <button type="button" onClick={() => change(false)} className={button}>
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
