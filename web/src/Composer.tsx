import { useState } from "react";
import type { Project } from "./api";
import { safeBranch, sameBranch } from "./branch";
import { BranchIcon, type Launch } from "./CreateWorkspace";
import { promptOf, sendKey, SparkIcon } from "./FirstPrompt";
import type { WorkspaceView } from "./fleet";
import { jumpKey, SearchIcon } from "./JumpPalette";
import { inlineField, primaryButton } from "./Modal";

/** A name made from a task takes this many words at most, and this many characters. */
const NAME_WORDS = 5;
const NAME_AT_MOST = 40;

/**
 * The workspace name a task gives: the first words of its first line, in lower case, cut at a
 * word to 40 characters, without the punctuation that closes a sentence. Empty for a blank task.
 */
export function taskName(task: string): string {
  const line = task.split("\n").find((l) => l.trim() !== "") ?? "";
  let name = "";
  for (const word of line.trim().toLowerCase().split(/\s+/).slice(0, NAME_WORDS)) {
    const next = name ? `${name} ${word}` : word;
    if (next.length > NAME_AT_MOST) break;
    name = next;
  }
  return name.replace(/[.!?,;:…]+$/u, "");
}

/** The workspace of `project` already on the branch `name` makes, if there is one. */
function holder(name: string, project: Project | undefined, workspaces: WorkspaceView[]) {
  const branch = safeBranch(name);
  return project ? workspaces.find((w) => w.project === project.name && sameBranch(w.branch, branch)) : undefined;
}

/** `name`, or `name 2`, `name 3`… when the project already has a workspace on its branch. */
function freeName(name: string, project: Project | undefined, workspaces: WorkspaceView[]): string {
  if (!name) return name;
  let next = name;
  for (let n = 2; holder(next, project, workspaces); n++) next = `${name} ${n}`;
  return next;
}

/**
 * The box a task is given to Claude in: the task names the workspace and its branch as it is
 * typed (numbered when the project already has that branch), the project, base and model sit
 * under it, and Start (or Cmd+Enter) makes the workspace on this machine and starts Claude Code on
 * the task, handing the session to `onCreated` so the layout lands in it.
 *
 * On the first-run page (`page`) it is open and big. At the top of the board (`bar`) it is one
 * line beside a way to jump until it is used, so starting the next task is a click away from
 * everything already running.
 */
export function Composer({
  mode,
  id,
  projects,
  workspaces,
  launch,
  onCreated,
  onMore,
  onJump,
  doneLabel,
}: {
  mode: "page" | "bar";
  /** The task box's id, so a heading elsewhere can label it. */
  id: string;
  projects: Project[];
  /** The workspaces open now, so a name a project already has is never sent. */
  workspaces: WorkspaceView[];
  launch: Launch;
  onCreated: (session: string) => void;
  /** Opens the full new-workspace dialog with this name. */
  onMore: (name: string) => void;
  /** Opens the jump palette, from the bar. */
  onJump?: () => void;
  /** The button that clears a workspace that was made but whose session did not start. */
  doneLabel: string;
}) {
  const [task, setTask] = useState("");
  const [name, setName] = useState("");
  const [base, setBase] = useState("main");
  const [model, setModel] = useState("opus");
  const [projectId, setProjectId] = useState(projects[0]?.id ?? "");
  const [focused, setFocused] = useState(false);
  const project = projects.find((p) => p.id === projectId) ?? projects[0];
  const prompt = promptOf(task);
  const suggested = freeName(taskName(task), project, workspaces);
  const chosen = name.trim() || suggested;
  const taken = name.trim() ? holder(name.trim(), project, workspaces) : undefined;
  const { create, step, made } = launch;
  // Once sent, the name is the one sent: the new workspace arriving must not renumber it.
  const shown = (create.isPending || made) && create.variables ? create.variables.name : chosen;
  const branch = safeBranch(shown);
  const ready = Boolean(project && chosen && base.trim() && model.trim()) && !taken && !create.isPending && !made;
  const bar = mode === "bar";
  const open = !bar || focused || task !== "" || name !== "" || create.isPending || create.isError;

  const submit = () => {
    if (!ready || !project) return;
    create.mutate({ project: project.id, base: base.trim(), name: chosen, model: model.trim(), prompt }, { onSuccess: ({ session }) => onCreated(session) });
  };
  const clear = () => {
    create.reset();
    setTask("");
    setName("");
  };

  return (
    <form
      onSubmit={(event) => {
        event.preventDefault();
        submit();
      }}
      onFocus={() => setFocused(true)}
      onBlur={(event) => {
        if (!event.currentTarget.contains(event.relatedTarget)) setFocused(false);
      }}
      className={bar ? "space-y-2" : "space-y-3"}
    >
      <div
        className={`rounded-2xl border bg-neutral-900 focus-within:border-neutral-400 ${
          bar ? "border-neutral-700/70 shadow-lg shadow-black/40 hover:border-neutral-500" : "border-neutral-700 shadow-2xl shadow-black/30"
        }`}
      >
        <div className="flex items-start">
          {bar && <SparkIcon className={`mt-3.5 ml-4 size-5 shrink-0 ${task.trim() ? "text-emerald-400" : "text-neutral-500"}`} />}
          <textarea
            id={id}
            name="task"
            aria-label={bar ? "New task" : undefined}
            autoFocus={!bar}
            rows={bar ? 1 : 3}
            value={task}
            onChange={(e) => setTask(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
                e.preventDefault();
                submit();
              } else if (e.key === "Escape" && bar && task === "") {
                e.currentTarget.blur();
              }
            }}
            placeholder={bar ? "Give Claude a new task…" : "Fix the login redirect, add a dark mode, tidy the README…"}
            className={`block w-full min-w-0 flex-1 resize-none bg-transparent text-base leading-relaxed text-neutral-100 outline-none [field-sizing:content] placeholder:text-neutral-600 ${
              bar ? `max-h-60 px-3 py-3 ${focused ? "placeholder:text-neutral-500" : "placeholder:text-neutral-400"}` : "max-h-72 min-h-28 rounded-t-2xl px-5 pt-4 pb-2 sm:text-lg"
            }`}
          />
          {bar && onJump && (
            <button
              type="button"
              onClick={onJump}
              aria-label="Jump to a session or shell"
              title="Jump to a session or shell"
              className="m-2 flex shrink-0 items-center gap-2 rounded-lg px-2.5 py-1.5 text-neutral-400 hover:bg-neutral-800 hover:text-neutral-100"
            >
              <SearchIcon className="size-4" />
              <kbd className="rounded border border-neutral-700 px-1.5 py-0.5 text-xs pointer-coarse:hidden">{jumpKey}</kbd>
            </button>
          )}
        </div>
        {open && (
          <div className="flex flex-wrap items-center gap-x-3 gap-y-2 border-t border-neutral-800 px-3 py-2.5 text-xs text-neutral-400">
            {projects.length > 0 && (
              <div role="radiogroup" aria-label="Project" className="flex flex-wrap gap-1">
                {projects.map((p) => {
                  const on = p.id === project?.id;
                  return (
                    <label
                      key={p.id}
                      className={`cursor-pointer rounded-full border px-2.5 py-1 font-medium has-[:focus-visible]:ring-2 has-[:focus-visible]:ring-neutral-400 ${
                        on ? "border-neutral-300 bg-neutral-100 text-neutral-900" : "border-neutral-700 text-neutral-300 hover:border-neutral-500"
                      }`}
                    >
                      <input type="radio" name="project" value={p.id} checked={on} onChange={() => setProjectId(p.id)} className="sr-only" />
                      {p.name}
                    </label>
                  );
                })}
              </div>
            )}
            <label className="flex items-center gap-1.5">
              from
              <input name="base" aria-label="Base" value={base} onChange={(e) => setBase(e.target.value)} style={{ width: `${Math.max(base.length, 4) + 2.5}ch` }} className={inlineField} />
            </label>
            <label className="flex items-center gap-1.5">
              with
              <input
                name="model"
                aria-label="Model"
                list={`${id}-models`}
                value={model}
                onChange={(e) => setModel(e.target.value)}
                style={{ width: `${Math.max(model.length, 4) + 4}ch` }}
                className={`${inlineField} [&::-webkit-calendar-picker-indicator]:hidden`}
              />
              <datalist id={`${id}-models`}>
                <option value="opus" />
                <option value="sonnet" />
              </datalist>
            </label>
            <span className="ml-auto flex items-center gap-2">
              <kbd className="rounded border border-neutral-700 px-1.5 py-0.5 text-[10px] text-neutral-500 pointer-coarse:hidden">{sendKey}</kbd>
              <button type="submit" disabled={!ready} className={`${primaryButton} px-3 py-1`}>
                Start
              </button>
            </span>
          </div>
        )}
      </div>

      {open && (
        <div className="flex flex-wrap items-center gap-x-2 gap-y-1.5 px-1 text-xs text-neutral-500">
          <BranchIcon className="size-3.5 shrink-0" />
          <label htmlFor={`${id}-name`}>Workspace</label>
          <input
            id={`${id}-name`}
            aria-label="Workspace name"
            autoComplete="off"
            spellCheck={false}
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder={suggested || "named from the task"}
            className={`${inlineField} w-48 max-w-full font-sans`}
          />
          {shown && (
            <>
              <span>on</span>
              <code aria-label="Branch" className={`min-w-0 truncate rounded px-1.5 py-0.5 font-mono ${taken ? "bg-amber-400/15 text-amber-300" : "bg-emerald-400/10 text-emerald-300"}`}>
                {branch}
              </code>
            </>
          )}
          <button type="button" onClick={() => onMore(chosen)} className="ml-auto text-neutral-400 underline decoration-neutral-700 underline-offset-4 hover:text-neutral-200">
            More options
          </button>
        </div>
      )}

      {taken && project && (
        <p className="px-1 text-xs text-amber-300">
          {project.name} already has a workspace on {taken.branch}. Pick another name.
        </p>
      )}
      {projects.length === 0 && (
        <p className="px-1 text-sm text-amber-300">No projects yet. A project is a repo and a name in the fleet's shared settings; once one is added, start its first task here.</p>
      )}
      <p aria-live="polite" className={`px-1 text-xs text-neutral-400 ${bar ? "empty:hidden" : "min-h-4"}`}>
        {create.isPending && (step === "branch" ? `Making ${branch} from ${base.trim()}…` : prompt ? `Starting ${model.trim()} on your task…` : `Starting ${model.trim()}…`)}
      </p>
      {create.isError && (
        <div role="alert" className="flex flex-wrap items-center gap-2 px-1 text-sm text-red-400">
          <span className="min-w-0 flex-1">{create.error?.message}</span>
          {made && (
            <button type="button" onClick={clear} className="text-neutral-300 underline decoration-neutral-600 underline-offset-4 hover:text-neutral-100">
              {doneLabel}
            </button>
          )}
        </div>
      )}
    </form>
  );
}
