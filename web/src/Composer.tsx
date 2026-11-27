import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { listMachines, type Project } from "./api";
import { safeBranch, sameBranch } from "./branch";
import { promptOf, sendKey, SparkIcon } from "./FirstPrompt";
import type { WorkspaceView } from "./fleet";
import { jumpKey, SearchIcon } from "./JumpPalette";
import { inlineField, primaryButton } from "./Modal";
import { makeWorkspace, SessionNotStarted, type CreateInput } from "./launch";
import { Projects } from "./Projects";
import { BranchIcon, leftOver, PlannedList, SplitButton, useStartEach, type Planned } from "./SplitTasks";
import { ITEM, listOf } from "./tasks";

/**
 * Makes a workspace, then starts a session in it, saying which `step` it is on. `made` is true
 * once the workspace exists, even when its session then failed to start, since a second try
 * would only find its name taken.
 */
export function useCreateWorkspace() {
  const [step, setStep] = useState<"branch" | "session">("branch");
  const client = useQueryClient();
  const create = useMutation({
    mutationFn: (input: CreateInput) => {
      setStep("branch");
      return makeWorkspace(input, client, () => setStep("session"));
    },
  });
  const made = create.isSuccess || create.error instanceof SessionNotStarted;
  return { create, step, made };
}

/** A create in hand: the mutation, its step and whether the workspace exists yet. */
export type Launch = ReturnType<typeof useCreateWorkspace>;

/** A name made from a task takes this many words at most, and this many characters. */
const NAME_WORDS = 5;
const NAME_AT_MOST = 40;

/**
 * The workspace name a task gives: the first words of its first line, without a list's mark, in
 * lower case, cut at a word to 40 characters, without the punctuation that closes a sentence.
 * Empty for a blank task.
 */
export function taskName(task: string): string {
  const line = task.split("\n").find((l) => l.trim() !== "") ?? "";
  let name = "";
  for (const word of line.trim().replace(ITEM, "").toLowerCase().split(/\s+/).slice(0, NAME_WORDS)) {
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

/** `name`, or `name 2`, `name 3`… when the project already has a workspace on its branch, or one of the names `also` taken has it. */
function freeName(name: string, project: Project | undefined, workspaces: WorkspaceView[], also: string[] = []): string {
  if (!name) return name;
  const used = (next: string) => holder(next, project, workspaces) || also.some((a) => sameBranch(safeBranch(a), safeBranch(next)));
  let next = name;
  for (let n = 2; used(next); n++) next = `${name} ${n}`;
  return next;
}

/** The workspace each item of a split task makes, named from its item and numbered past the project's branches and the items before it. */
function planOf(items: string[], project: Project | undefined, workspaces: WorkspaceView[]): Planned[] {
  const plan: Planned[] = [];
  for (const prompt of items) {
    plan.push({ name: freeName(taskName(prompt), project, workspaces, plan.map((p) => p.name)), prompt });
  }
  return plan;
}

/**
 * The box a task is given to Claude in: the task names the workspace and its branch as it is
 * typed (numbered when the project already has that branch), the project, base and model sit
 * under it, and Start (or Cmd+Enter) makes the workspace and starts Claude Code on the task,
 * handing the session to `onCreated` so the layout lands in it. The workspace is made on this
 * machine, or on another listed one when the fleet has one and it is picked, or on the one
 * Foundry picks when it is let choose; an offline machine is shown but cannot be picked. When
 * the machine list cannot be read, only this machine is offered.
 *
 * On the first-run page and in the New task dialog (`page`) it is open and big. At the top of
 * the board (`bar`) it is one line beside a way to jump until it is used, so starting the next
 * task is a click away from everything already running. It is the one way a workspace is made.
 *
 * A task written as a list offers to split: each item then makes its own workspace and session
 * with the same project, base, model and machine, one after another, and none is landed in.
 */
export function Composer({
  mode,
  id,
  projects,
  workspaces,
  launch,
  onCreated,
  onJump,
  doneLabel,
  onDone,
  initialTask = "",
  initialMachine = "",
}: {
  mode: "page" | "bar";
  /** The task box's id, so a heading elsewhere can label it. */
  id: string;
  projects: Project[];
  /** The workspaces open now, so a name a project already has is never sent. */
  workspaces: WorkspaceView[];
  launch: Launch;
  /** Called with the new session, and the daemon's words when the workspace's setup did not start. */
  onCreated: (session: string, warning: string | null) => void;
  /** Opens the jump palette, from the bar. */
  onJump?: () => void;
  /** The button shown once a workspace was made but went wrong: its session or its setup did not start. */
  doneLabel: string;
  /** What that button does; by default it clears the box for the next task. */
  onDone?: () => void;
  /** The task the box opens with. */
  initialTask?: string;
  /** The machine the box opens set to: empty for this machine, `"choose"`, or another machine's id. */
  initialMachine?: string;
}) {
  const [task, setTask] = useState(initialTask);
  const [name, setName] = useState("");
  const [base, setBase] = useState("main");
  const [model, setModel] = useState("opus");
  const [projectId, setProjectId] = useState(projects[0]?.id ?? "");
  /** The machine picked: empty for this machine. */
  const [machine, setMachine] = useState(initialMachine);
  const [focused, setFocused] = useState(false);
  /** Whether the Projects dialog is open. */
  const [managing, setManaging] = useState(false);
  /** Whether a task written as a list makes a workspace per item. */
  const [split, setSplit] = useState(false);
  const each = useStartEach();
  const project = projects.find((p) => p.id === projectId) ?? projects[0];
  const prompt = promptOf(task);
  const suggested = freeName(taskName(task), project, workspaces);
  const chosen = name.trim() || suggested;
  const taken = name.trim() ? holder(name.trim(), project, workspaces) : undefined;
  const { create, step, made } = launch;
  const warning = create.data?.warning ?? null;
  const models = [...new Set(["opus", "sonnet", ...workspaces.flatMap((w) => w.sessions.map((s) => s.model))])];
  // Once sent, the name is the one sent: the new workspace arriving must not renumber it.
  const shown = (create.isPending || made) && create.variables ? create.variables.name : chosen;
  const branch = safeBranch(shown);
  const items = listOf(task);
  // While a split start runs, the plan is the one sent, so workspaces arriving do not renumber it.
  const sent = each.run.isPending ? each.run.variables : undefined;
  const plan = sent ? sent.map((input) => ({ name: input.name, prompt: input.prompt ?? "" })) : split && items ? planOf(items, project, workspaces) : undefined;
  const busy = create.isPending || each.run.isPending;
  const ready = plan
    ? Boolean(project && base.trim() && model.trim()) && plan.every((p) => p.name) && !busy
    : Boolean(project && chosen && base.trim() && model.trim()) && !taken && !busy && !made;
  const bar = mode === "bar";
  const open = !bar || focused || managing || task !== "" || name !== "" || busy || create.isError || each.run.isSuccess;
  const ended = each.run.data;
  const trouble = ended && (ended.missed.length > 0 || ended.warned.length > 0) ? ended : undefined;
  // Read only once the box is open: the board's closed bar shows no machine.
  const listed = useQuery({ queryKey: ["machines"], queryFn: listMachines, retry: false, enabled: open });
  const others = listed.data ? listed.data.machines.filter((m) => m.id !== listed.data.me) : [];

  const submit = () => {
    if (!ready || !project) return;
    each.run.reset();
    if (plan) {
      const inputs = plan.map((p) => ({ project: project.id, base: base.trim(), name: p.name, machine: machine || undefined, model: model.trim(), prompt: p.prompt }));
      each.run.mutate(inputs, {
        // Each one shows up on the board as it starts, so nothing is landed in; what did not start stays in the box.
        onSuccess: ({ missed, warned }) => (missed.length === 0 && warned.length === 0 ? (onDone ?? clear)() : setTask(leftOver(missed))),
      });
      return;
    }
    create.mutate(
      { project: project.id, base: base.trim(), name: chosen, machine: machine || undefined, model: model.trim(), prompt },
      {
        // A request that waits is shown in the waiting list; the box is done with it.
        onSuccess: ({ session, warning }) => (session ? onCreated(session, warning) : (onDone ?? clear)()),
      },
    );
  };
  const clear = () => {
    create.reset();
    each.run.reset();
    setTask("");
    setName("");
    setSplit(false);
  };

  return (
    <>
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
              <button
                type="button"
                onClick={() => setManaging(true)}
                className="rounded-full border border-dashed border-neutral-700 px-2.5 py-1 text-neutral-400 hover:border-neutral-500 hover:text-neutral-200"
              >
                Projects
              </button>
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
                  {models.map((m) => (
                    <option key={m} value={m} />
                  ))}
                </datalist>
              </label>
              {others.length === 0 ? (
                <span>on this machine</span>
              ) : (
                <label className="flex items-center gap-1.5">
                  on
                  <select name="machine" aria-label="Machine" value={machine} onChange={(e) => setMachine(e.target.value)} className={`${inlineField} font-sans`}>
                    <option value="">this machine</option>
                    <option value="choose">let Foundry choose</option>
                    {others.map((m) => (
                      <option key={m.id} value={m.id} disabled={!m.online}>
                        {m.online ? m.name : `${m.name} (offline)`}
                      </option>
                    ))}
                  </select>
                </label>
              )}
              <span className="ml-auto flex items-center gap-2">
                <kbd className="rounded border border-neutral-700 px-1.5 py-0.5 text-[10px] text-neutral-500 pointer-coarse:hidden">{sendKey}</kbd>
                <button type="submit" disabled={!ready} className={`${primaryButton} px-3 py-1`}>
                  {plan ? `Start ${plan.length}` : "Start"}
                </button>
              </span>
            </div>
          )}
        </div>

        {open && plan && (
          <div className="space-y-2">
            <div className="flex flex-wrap items-center gap-x-2 gap-y-1.5 px-1 text-xs text-neutral-500">
              <SplitButton count={plan.length} split disabled={busy} onToggle={() => setSplit(false)} />
              <span>A workspace for each item, with Claude at work on it.</span>
            </div>
            <PlannedList plan={plan} at={sent ? each.at : undefined} started={each.started} />
          </div>
        )}
        {open && !plan && (
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
            {items && <SplitButton count={items.length} split={false} disabled={busy} onToggle={() => setSplit(true)} />}
          </div>
        )}

        {taken && project && (
          <p className="px-1 text-xs text-amber-300">
            {project.name} already has a workspace on {taken.branch}. Pick another name.
          </p>
        )}
        {projects.length === 0 && (
          <p className="px-1 text-sm text-amber-300">
            No projects yet. A project is a repo and a name in the fleet's shared settings; once one is added, start its first task here.{" "}
            <button type="button" onClick={() => setManaging(true)} className="font-medium text-amber-200 underline decoration-amber-400/50 underline-offset-4 hover:text-amber-100">
              Add a project
            </button>
          </p>
        )}
        <p aria-live="polite" className={`px-1 text-xs text-neutral-400 ${bar ? "empty:hidden" : "min-h-4"}`}>
          {create.isPending && (step === "branch" ? `Making ${branch} from ${base.trim()}…` : prompt ? `Starting ${model.trim()} on your task…` : `Starting ${model.trim()}…`)}
          {sent && `Starting ${each.at + 1} of ${sent.length}: ${sent[each.at]?.name}…`}
        </p>
        {trouble && (
          <div role={trouble.missed.length > 0 ? "alert" : "status"} className={`flex flex-wrap items-start gap-2 px-1 text-sm ${trouble.missed.length > 0 ? "text-red-400" : "text-amber-300"}`}>
            <div className="min-w-0 flex-1 space-y-0.5">
              <p>
                Started {trouble.of - trouble.missed.length} of {trouble.of}.
              </p>
              {trouble.missed.map((m) => (
                <p key={m.name}>
                  {m.name}: {m.why}
                </p>
              ))}
              {trouble.warned.map((w) => (
                <p key={w} className="text-amber-300">
                  {w}
                </p>
              ))}
            </div>
            <button type="button" onClick={onDone ?? clear} className="text-neutral-300 underline decoration-neutral-600 underline-offset-4 hover:text-neutral-100">
              {doneLabel}
            </button>
          </div>
        )}
        {(create.isError || warning) && (
          <div role={create.isError ? "alert" : "status"} className={`flex flex-wrap items-center gap-2 px-1 text-sm ${create.isError ? "text-red-400" : "text-amber-300"}`}>
            <span className="min-w-0 flex-1">{create.isError ? create.error?.message : warning}</span>
            {made && (
              <button type="button" onClick={onDone ?? clear} className="text-neutral-300 underline decoration-neutral-600 underline-offset-4 hover:text-neutral-100">
                {doneLabel}
              </button>
            )}
          </div>
        )}
      </form>
      <Projects open={managing} onOpenChange={setManaging} projects={projects} onAdded={setProjectId} />
    </>
  );
}
