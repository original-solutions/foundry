import type { Project } from "./api";
import { Composer, type Launch } from "./Composer";
import type { WorkspaceView } from "./fleet";
import type { Glyph } from "./labels";
import { Mark } from "./Mark";
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
  /** Called with the new session's id once it has started, so the layout can open it. */
  onCreated: (session: string) => void;
  /** Shows the sidebar, on a screen too narrow to keep it beside. */
  onMenu: () => void;
}) {
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
            Each task gets a branch and a folder of its own, with Claude Code already at work in it. You follow it from here.
          </p>
        </div>

        <Composer mode="page" id="launch-task" projects={projects} workspaces={workspaces} launch={launch} onCreated={(session) => onCreated(session)} doneLabel="Go to the board" />

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
