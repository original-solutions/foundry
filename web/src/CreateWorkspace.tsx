import { useMutation, useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { Dialog } from "radix-ui";
import { ApiError, createWorkspace, listMachines, startSession, type Project } from "./api";
import { safeBranch, sameBranch } from "./branch";
import { FirstPrompt, promptOf } from "./FirstPrompt";
import type { WorkspaceView } from "./fleet";
import { plural } from "./labels";
import { bigField, button, field, inlineField, Modal, primaryButton } from "./Modal";

/** The workspace was made, but its session did not start. */
class SessionNotStarted extends ApiError {}

type Step = "branch" | "session";

/** What the form asks for. `machine` is absent for this machine. */
interface CreateInput {
  project: string;
  base: string;
  name: string;
  model: string;
  /** The first prompt, when one was typed. */
  prompt?: string;
  machine?: string;
}

interface Props {
  projects: Project[];
  /** The workspaces open now, so a name whose branch a project already has is caught here. */
  workspaces: WorkspaceView[];
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** The name the dialog starts with, as typed in the jump palette. */
  name?: string;
  /** Called with the new session's id once it has started, so the layout can open it. */
  onCreated?: (session: string) => void;
}

/**
 * The new-workspace dialog: a name, which shows the branch it makes as you type, then the project,
 * base, model and machine. Create makes the workspace, starts a session in it and hands the
 * session to `onCreated`, so the layout lands in it.
 */
export function CreateWorkspace({ projects, workspaces, open, onOpenChange, name = "", onCreated }: Props) {
  const [step, setStep] = useState<Step>("branch");
  const create = useMutation({
    mutationFn: async (input: CreateInput) => {
      setStep("branch");
      const workspace = await createWorkspace({ project: input.project, base: input.base, name: input.name, machine: input.machine });
      setStep("session");
      // The workspace is usable when its setup did not start, so its session is started all the same.
      let session: string;
      try {
        session = await startSession(workspace.id, input.model, input.prompt);
      } catch (error) {
        const setup = workspace.error ? ` Also: ${workspace.error}` : "";
        throw new SessionNotStarted(`The workspace was made, but its session did not start: ${error instanceof Error ? error.message : error}.${setup}`);
      }
      return { session, warning: workspace.error ?? null };
    },
  });
  const change = (next: boolean) => {
    onOpenChange(next);
    create.reset();
  };

  return (
    <Modal open={open} onOpenChange={change} wide title="New workspace" description="A branch of its own, with Claude Code already running in it, and already at work when you give it a first prompt.">
      <Form
        projects={projects}
        workspaces={workspaces}
        initialName={name}
        create={create}
        step={step}
        onSubmit={(input) =>
          create.mutate(input, {
            // Given per call, so a call the dialog was reset from cannot close it.
            onSuccess: ({ session, warning }) => {
              onCreated?.(session);
              if (!warning) onOpenChange(false);
            },
          })
        }
      />
    </Modal>
  );
}

interface FormProps {
  projects: Project[];
  workspaces: WorkspaceView[];
  initialName: string;
  create: {
    isPending: boolean;
    isSuccess: boolean;
    isError: boolean;
    error: Error | null;
    data: { warning: string | null } | undefined;
  };
  step: Step;
  onSubmit: (input: CreateInput) => void;
}

/** Mounted while the dialog is open, so each opening starts from what it was given. */
function Form({ projects, workspaces, initialName, create, step, onSubmit }: FormProps) {
  const [name, setName] = useState(initialName);
  const [base, setBase] = useState("main");
  const [model, setModel] = useState("opus");
  const [prompt, setPrompt] = useState("");
  const first = promptOf(prompt);
  const [projectId, setProjectId] = useState(projects[0]?.id ?? "");
  /** The machine picked: empty for this machine. */
  const [machine, setMachine] = useState("");
  const listed = useQuery({ queryKey: ["machines"], queryFn: listMachines, retry: false });
  const others = listed.data ? listed.data.machines.filter((m) => m.id !== listed.data.me) : [];
  const project = projects.find((p) => p.id === projectId) ?? projects[0];
  const branch = safeBranch(name);
  const taken = project && name.trim() !== "" ? workspaces.find((w) => w.project === project.name && sameBranch(w.branch, branch)) : undefined;
  const warning = create.data?.warning ?? null;
  // Once the workspace exists, a second create would only find its name taken, so the dialog
  // then offers Close alone, with what went wrong.
  const made = create.isSuccess || create.error instanceof SessionNotStarted;
  const models = [...new Set(["opus", "sonnet", ...workspaces.flatMap((w) => w.sessions.map((s) => s.model))])];

  return (
    <form
      className="mt-4 space-y-4"
      onSubmit={(event) => {
        event.preventDefault();
        if (!project || taken) return;
        onSubmit({ project: project.id, base, name, model, prompt: first, machine: machine || undefined });
      }}
    >
      <div>
        <label htmlFor="new-workspace-name" className="sr-only">
          Name
        </label>
        <input
          id="new-workspace-name"
          name="name"
          autoFocus
          required
          autoComplete="off"
          spellCheck={false}
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="What are you working on?"
          className={bigField}
        />
        <div className="mt-2 flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-neutral-500">
          <BranchIcon className="size-3.5 shrink-0" />
          {name.trim() ? (
            <code aria-label="Branch" className={`min-w-0 truncate rounded px-1.5 py-0.5 font-mono ${taken ? "bg-amber-400/15 text-amber-300" : "bg-emerald-400/10 text-emerald-300"}`}>
              {branch}
            </code>
          ) : (
            <span>a new branch</span>
          )}
          <label className="flex items-center gap-1.5">
            from
            <input
              name="base"
              aria-label="Base"
              required
              value={base}
              onChange={(e) => setBase(e.target.value)}
              size={Math.max(base.length, 4)}
              className={inlineField}
            />
          </label>
        </div>
        {taken && (
          <p className="mt-1.5 text-xs text-amber-300">
            {project.name} already has a workspace on {taken.branch}. Pick another name.
          </p>
        )}
      </div>

      <FirstPrompt id="new-workspace-prompt" value={prompt} onChange={setPrompt} />

      {projects.length === 0 ? (
        <p>No projects yet</p>
      ) : (
        <div role="radiogroup" aria-labelledby="new-workspace-project">
          <div id="new-workspace-project" className="mb-1.5 text-xs text-neutral-400">
            Project
          </div>
          <div className="flex flex-wrap gap-1.5">
            {projects.map((p) => {
              const count = workspaces.filter((w) => w.project === p.name).length;
              const on = p.id === project?.id;
              return (
                <label
                  key={p.id}
                  title={`${plural(count, "workspace")} open`}
                  className={`flex cursor-pointer items-center gap-2 rounded-lg border px-3 py-1.5 has-[:focus-visible]:ring-2 has-[:focus-visible]:ring-neutral-400 ${
                    on ? "border-neutral-300 bg-neutral-100 text-neutral-900" : "border-neutral-700 text-neutral-300 hover:border-neutral-500"
                  }`}
                >
                  <input type="radio" name="project" value={p.id} checked={on} onChange={() => setProjectId(p.id)} className="sr-only" />
                  <span className="font-medium">{p.name}</span>
                  <span className="text-xs tabular-nums text-neutral-500">{count}</span>
                </label>
              );
            })}
          </div>
        </div>
      )}

      <div className="grid grid-cols-2 gap-3">
        <label className="block text-xs text-neutral-400">
          Model
          <input name="model" list="new-workspace-models" className={field} value={model} onChange={(e) => setModel(e.target.value)} required />
          <datalist id="new-workspace-models">
            {models.map((m) => (
              <option key={m} value={m} />
            ))}
          </datalist>
        </label>
        <label className="block text-xs text-neutral-400">
          Machine
          <select name="machine" className={field} value={machine} onChange={(e) => setMachine(e.target.value)}>
            <option value="">This machine</option>
            {others.map((m) => (
              <option key={m.id} value={m.id} disabled={!m.online}>
                {m.online ? m.name : `${m.name} (offline)`}
              </option>
            ))}
          </select>
        </label>
      </div>

      {create.isError && (
        <p role="alert" className="text-red-400">
          {create.error?.message}
        </p>
      )}
      {warning && (
        <p role="status" className="text-amber-400">
          {warning}
        </p>
      )}
      <div className="flex items-center justify-end gap-2 border-t border-neutral-800 pt-3">
        <p aria-live="polite" className="mr-auto min-w-0 truncate text-xs text-neutral-400">
          {create.isPending && (step === "branch" ? `Making ${branch} from ${base}…` : first ? `Starting ${model} on your prompt…` : `Starting ${model}…`)}
        </p>
        <Dialog.Close className={button}>{made ? "Close" : "Cancel"}</Dialog.Close>
        {!made && (
          <button
            type="submit"
            disabled={projects.length === 0 || create.isPending || Boolean(taken)}
            className={primaryButton}
          >
            {first ? "Create and start" : "Create and open"}
          </button>
        )}
      </div>
    </form>
  );
}

function BranchIcon({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 16 16" className={className} fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
      <circle cx="4.5" cy="3.5" r="1.5" />
      <circle cx="4.5" cy="12.5" r="1.5" />
      <circle cx="11.5" cy="5.5" r="1.5" />
      <path d="M4.5 5v6M11.5 7c0 3-7 2-7 4" strokeLinecap="round" />
    </svg>
  );
}
