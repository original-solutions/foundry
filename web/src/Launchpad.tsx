import { useState, type ReactNode } from "react";
import type { Project } from "./api";
import { Composer, type Launch } from "./Composer";
import { FirstRepo } from "./FirstRepo";
import type { WorkspaceView } from "./fleet";
import type { Glyph } from "./labels";
import { Mark } from "./Mark";
import { ProjectFace } from "./ProjectFace";
import { glyphText, StateGlyph } from "./StateGlyph";

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
 *
 * A fleet with no project yet gets there in two steps: first the repo Claude will work in, then
 * the task. Once the project arrives in the settings the first step is ticked off with its face
 * and the task box opens on it, so the steps stay until a task starts.
 */
export function Launchpad({
  projects,
  workspaces,
  launch,
  onCreated,
  onMenu,
}: {
  projects: Project[];
  /** The workspaces open now: a create under way keeps this page while its workspace arrives. */
  workspaces: WorkspaceView[];
  launch: Launch;
  /** Called with the new session's id once it has started, so the layout can open it, and the daemon's warning about its create, if any. */
  onCreated: (session: string, warning: string | null) => void;
  /** Shows the sidebar, on a screen too narrow to keep it beside. */
  onMenu: () => void;
}) {
  /** The project added from this page, so its steps stay on screen once it arrives. */
  const [added, setAdded] = useState<string>();
  const steps = projects.length === 0 || added !== undefined;
  const repo = projects.find((p) => p.id === added) ?? projects[0];
  const composer = <Composer mode="page" id="launch-task" projects={projects} workspaces={workspaces} launch={launch} onCreated={(session, warning) => onCreated(session, warning)} doneLabel="Go to the board" />;

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
            {steps ? "Two steps to Claude at work" : <label htmlFor="launch-task">What should Claude work on?</label>}
          </h2>
          <p className="max-w-xl text-sm leading-relaxed text-pretty text-neutral-400">
            Each task gets a branch and a folder of its own, with Claude Code already at work in it. You follow it from here.
          </p>
        </div>

        {steps ? (
          <ol aria-label="Getting started" className="space-y-2">
            <Step n={1} at={repo ? "done" : "now"} title="Which repo should Claude work in?">
              {repo ? (
                <p className="flex min-w-0 items-center gap-2 text-sm">
                  <ProjectFace project={repo.name} className="size-5 text-[9px]" />
                  <span className="shrink-0 font-medium text-neutral-100">{repo.name}</span>
                  <span className="min-w-0 truncate font-mono text-xs text-neutral-500" title={repo.repo}>
                    {repo.repo}
                  </span>
                </p>
              ) : (
                <FirstRepo onAdded={setAdded} />
              )}
            </Step>
            <Step n={2} at={repo ? "now" : "next"} title="What should Claude work on?" labels={repo ? "launch-task" : undefined} last>
              {repo ? composer : <p className="text-sm text-neutral-500">Once the repo is in, the task goes here. Claude starts on it in a branch of its own.</p>}
            </Step>
          </ol>
        ) : (
          composer
        )}

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

/**
 * One step of the first run: its number (a tick once done) on a line down to the next, its
 * question, and what answers it. The step under way is the list's current one; a later one is dim.
 */
function Step({ n, at, title, labels, last = false, children }: { n: number; at: "done" | "now" | "next"; title: string; labels?: string; last?: boolean; children: ReactNode }) {
  return (
    <li aria-current={at === "now" ? "step" : undefined} className="relative grid grid-cols-[1.75rem_1fr] gap-x-3 gap-y-3 pb-4">
      {!last && <span aria-hidden="true" className={`absolute top-8 bottom-0 left-[0.8125rem] hidden w-px sm:block ${at === "done" ? "bg-emerald-500/50" : "bg-neutral-800"}`} />}
      <span
        aria-hidden="true"
        className={`grid size-7 place-items-center rounded-full border text-xs font-semibold ${
          at === "done"
            ? "border-emerald-500 bg-emerald-500 text-neutral-950 motion-safe:animate-state-in"
            : at === "now"
              ? "border-emerald-400 text-emerald-300 shadow-[0_0_0_4px] shadow-emerald-400/15"
              : "border-neutral-700 text-neutral-500"
        }`}
      >
        {at === "done" ? (
          <svg viewBox="0 0 16 16" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="2.25" strokeLinecap="round" strokeLinejoin="round">
            <path d="M3.5 8.5 6.5 11.5 12.5 4.5" />
          </svg>
        ) : (
          n
        )}
      </span>
      <h3 className={`self-center text-lg font-semibold tracking-tight ${at === "next" ? "text-neutral-500" : "text-neutral-50"}`}>
        {labels ? <label htmlFor={labels}>{title}</label> : title}
        {at === "done" && <span className="sr-only"> (done)</span>}
      </h3>
      <div className="col-span-2 min-w-0 sm:col-start-2 sm:col-span-1">{children}</div>
    </li>
  );
}
