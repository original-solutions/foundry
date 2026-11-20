import { useState } from "react";
import type { Project } from "./api";
import { safeBranch } from "./branch";
import { BranchIcon, type Launch } from "./CreateWorkspace";
import { promptOf, sendKey } from "./FirstPrompt";
import type { Glyph } from "./labels";
import { Mark } from "./Mark";
import { inlineField, primaryButton } from "./Modal";
import { glyphText, StateGlyph } from "./StateGlyph";

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

/** What a task shows once it starts: the three states a new user meets first. */
const states: { glyph: Glyph; title: string; text: string }[] = [
  { glyph: "working", title: "Working", text: "Claude is on it. Its dots ripple for as long as it runs." },
  { glyph: "waiting", title: "Needs you", text: "It asked something or wants a permission. Its row turns amber." },
  { glyph: "ready", title: "Ready", text: "Its turn is done. Look over its changes, then give it the next thing." },
];

/**
 * The home of a fleet with no workspaces yet: one question and a box to answer it in. The task
 * names the workspace and its branch as it is typed; Start makes the workspace in the picked
 * project and starts Claude Code on the task, so the first thing a new user does is put an agent
 * to work. Under it, the three states that task will show. The create is the layout's, so it
 * outlives this page when the new workspace arrives before its session has started.
 */
export function Launchpad({
  projects,
  launch,
  onCreated,
  onMore,
  onMenu,
}: {
  projects: Project[];
  launch: Launch;
  /** Called with the new session's id once it has started, so the layout can open it. */
  onCreated: (session: string) => void;
  /** Opens the full new-workspace dialog with this name. */
  onMore: (name: string) => void;
  /** Shows the sidebar, on a screen too narrow to keep it beside. */
  onMenu: () => void;
}) {
  const [task, setTask] = useState("");
  const [name, setName] = useState("");
  const [base, setBase] = useState("main");
  const [model, setModel] = useState("opus");
  const [projectId, setProjectId] = useState(projects[0]?.id ?? "");
  const project = projects.find((p) => p.id === projectId) ?? projects[0];
  const prompt = promptOf(task);
  const suggested = taskName(task);
  const chosen = name.trim() || suggested;
  const branch = safeBranch(chosen);
  const { create, step, made } = launch;
  const ready = Boolean(project && chosen && base.trim() && model.trim()) && !create.isPending && !made;

  const submit = () => {
    if (!ready || !project) return;
    create.mutate({ project: project.id, base: base.trim(), name: chosen, model: model.trim(), prompt }, { onSuccess: ({ session }) => onCreated(session) });
  };

  return (
    <div className="absolute inset-0 overflow-y-auto">
      <button
        type="button"
        onClick={onMenu}
        aria-label="Show the sidebar"
        className="absolute top-3 left-3 rounded p-1.5 text-neutral-400 hover:bg-neutral-800 hover:text-neutral-100 md:hidden"
      >
        <svg viewBox="0 0 16 16" className="size-4" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
          <path d="M2.5 4h11M2.5 8h11M2.5 12h11" strokeLinecap="round" />
        </svg>
      </button>
      <div className="mx-auto flex min-h-full w-full max-w-2xl flex-col justify-center gap-7 px-4 pt-14 pb-12 sm:px-6 md:pt-10">
        <div className="space-y-3">
          <div className="flex items-center gap-2.5 text-neutral-400">
            <Mark className="size-6 text-emerald-400" />
            <span className="text-xs font-semibold tracking-[0.25em] uppercase">Foundry</span>
          </div>
          <h2 className="text-3xl leading-tight font-semibold tracking-tight text-balance text-neutral-50 sm:text-4xl">
            <label htmlFor="launch-task">What should Claude work on?</label>
          </h2>
          <p className="max-w-xl text-sm leading-relaxed text-pretty text-neutral-400">
            Each task gets a branch and a folder of its own on this machine, with Claude Code already at work in it. You follow it from here.
          </p>
        </div>

        <form
          onSubmit={(event) => {
            event.preventDefault();
            submit();
          }}
          className="space-y-3"
        >
          <div className="group rounded-2xl border border-neutral-700 bg-neutral-900 shadow-2xl shadow-black/30 focus-within:border-neutral-400">
            <textarea
              id="launch-task"
              name="task"
              autoFocus
              rows={3}
              value={task}
              onChange={(e) => setTask(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
                  e.preventDefault();
                  submit();
                }
              }}
              placeholder="Fix the login redirect, add a dark mode, tidy the README…"
              className="block max-h-72 min-h-28 w-full resize-none rounded-t-2xl bg-transparent px-5 pt-4 pb-2 text-base leading-relaxed text-neutral-100 outline-none [field-sizing:content] placeholder:text-neutral-600 sm:text-lg"
            />
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
                <input name="model" aria-label="Model" list="launch-models" value={model} onChange={(e) => setModel(e.target.value)} style={{ width: `${Math.max(model.length, 4) + 4}ch` }} className={`${inlineField} [&::-webkit-calendar-picker-indicator]:hidden`} />
                <datalist id="launch-models">
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
          </div>

          <div className="flex flex-wrap items-center gap-x-2 gap-y-1.5 px-1 text-xs text-neutral-500">
            <BranchIcon className="size-3.5 shrink-0" />
            <label htmlFor="launch-name">Workspace</label>
            <input
              id="launch-name"
              aria-label="Workspace name"
              autoComplete="off"
              spellCheck={false}
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder={suggested || "named from the task"}
              className={`${inlineField} w-48 max-w-full font-sans`}
            />
            {chosen && (
              <>
                <span>on</span>
                <code aria-label="Branch" className="min-w-0 truncate rounded bg-emerald-400/10 px-1.5 py-0.5 font-mono text-emerald-300">
                  {branch}
                </code>
              </>
            )}
            <button
              type="button"
              onClick={() => onMore(chosen)}
              className="ml-auto text-neutral-400 underline decoration-neutral-700 underline-offset-4 hover:text-neutral-200"
            >
              More options
            </button>
          </div>

          {projects.length === 0 && (
            <p className="px-1 text-sm text-amber-300">
              No projects yet. A project is a repo and a name in the fleet's shared settings; once one is added, start its first task here.
            </p>
          )}
          <p aria-live="polite" className="min-h-4 px-1 text-xs text-neutral-400">
            {create.isPending && (step === "branch" ? `Making ${branch} from ${base.trim()}…` : prompt ? `Starting ${model.trim()} on your task…` : `Starting ${model.trim()}…`)}
          </p>
          {create.isError && (
            <div role="alert" className="flex flex-wrap items-center gap-2 px-1 text-sm text-red-400">
              <span className="min-w-0 flex-1">{create.error?.message}</span>
              {made && (
                <button type="button" onClick={() => create.reset()} className="text-neutral-300 underline decoration-neutral-600 underline-offset-4 hover:text-neutral-100">
                  Go to the board
                </button>
              )}
            </div>
          )}
        </form>

        <ul aria-label="What you will see" className="grid gap-px overflow-hidden rounded-xl border border-neutral-800 bg-neutral-800 sm:grid-cols-3">
          {states.map((s) => (
            <li key={s.glyph} className="flex gap-3 bg-neutral-950 p-4 sm:flex-col sm:gap-2">
              <span className="pt-0.5 [&_svg]:size-5">
                <StateGlyph glyph={s.glyph} still />
              </span>
              <span className="space-y-1">
                <span className={`block text-sm font-medium ${glyphText[s.glyph]}`}>{s.title}</span>
                <span className="block text-xs leading-relaxed text-neutral-400">{s.text}</span>
              </span>
            </li>
          ))}
        </ul>
      </div>
    </div>
  );
}
