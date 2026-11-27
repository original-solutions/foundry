import { useMutation, useQueryClient } from "@tanstack/react-query";
import { createContext, useContext, useEffect, useRef, useState } from "react";
import { safeBranch } from "./branch";
import { makeWorkspace, SessionNotStarted, type CreateInput } from "./launch";
import { buttonShape, pressed } from "./Modal";

/** One item of a split task: the workspace it makes and the prompt its session starts on. */
export interface Planned {
  name: string;
  /** Names to try in turn while the daemon refuses the one before as taken. */
  then?: string[];
  prompt: string;
}

/** An item that did not start, and why; `made` when its workspace exists all the same. */
interface Missed {
  name: string;
  prompt?: string;
  why: string;
  made: boolean;
}

/**
 * Where a split start says how it ended when the box that started it closed meanwhile (the dialog
 * shut, or the page under it changed), so the items that did not start are never lost unseen. The
 * layout shows it as its task warning.
 */
export const SplitEnded = createContext<((text: string) => void) | undefined>(undefined);

/** How a split start ended, in words: how many started, then each item that did not and why, then each warning. */
export function endedText({ of, missed, warned }: { of: number; missed: Missed[]; warned: string[] }): string {
  return [`Started ${of - missed.length} of ${of} split tasks.`, ...missed.map((m) => `${m.name}: ${m.why}`), ...warned].join("\n");
}

/**
 * Starts the items of a split task one after another, each as its own workspace and session, so
 * a project's git is never asked for two worktrees at once. An item that fails does not stop the
 * rest. `at` is the item being made, and `started` holds, for each item done, whether it started.
 * When the box is gone by the time the last item is done and any did not start cleanly, the
 * ending goes to [`SplitEnded`] instead.
 */
export function useStartEach() {
  const client = useQueryClient();
  const report = useContext(SplitEnded);
  const mounted = useRef(false);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  const [at, setAt] = useState(0);
  const [started, setStarted] = useState<boolean[]>([]);
  const run = useMutation({
    mutationFn: async (inputs: CreateInput[]) => {
      setStarted([]);
      const missed: Missed[] = [];
      const warned: string[] = [];
      for (const [i, input] of inputs.entries()) {
        setAt(i);
        try {
          const { warning } = await makeWorkspace(input, client);
          if (warning) warned.push(`${input.name}: ${warning}`);
          setStarted((done) => [...done, true]);
        } catch (error) {
          missed.push({ name: input.name, prompt: input.prompt, why: error instanceof Error ? error.message : String(error), made: error instanceof SessionNotStarted });
          setStarted((done) => [...done, false]);
        }
      }
      const ended = { of: inputs.length, missed, warned };
      if (!mounted.current && (missed.length > 0 || warned.length > 0)) report?.(endedText(ended));
      return ended;
    },
  });
  return { run, at, started };
}

/** The task left in the box after a split start: the items whose workspace was not made, as a list again when there are two or more. */
export function leftOver(missed: Missed[]): string {
  const left = missed.filter((m) => !m.made).map((m) => m.prompt ?? m.name);
  if (left.length === 1) return left[0];
  return left.map((prompt) => `- ${prompt.replace(/\n/g, "\n  ")}`).join("\n");
}

/** The button that splits a task written as a list into one workspace per item, or joins it again. */
export function SplitButton({ count, split, disabled, onToggle }: { count: number; split: boolean; disabled?: boolean; onToggle: () => void }) {
  return (
    <button
      type="button"
      aria-pressed={split}
      disabled={disabled}
      onClick={onToggle}
      title={split ? "Make one workspace with the whole list as its task" : "Make a workspace per item, each with Claude at work on it"}
      className={`flex shrink-0 items-center gap-1.5 ${buttonShape} ${pressed} border-neutral-700 text-neutral-300 hover:bg-neutral-800 motion-safe:animate-state-in`}
    >
      <ForkIcon className="size-3.5" />
      Split into {count} tasks
    </button>
  );
}

/**
 * The workspaces a split task makes, in order: each one's branch and the first line of its
 * prompt. While they start, the one being made pulses and the ones done are ticked or crossed.
 */
export function PlannedList({ plan, at, started }: { plan: Planned[]; at?: number; started: boolean[] }) {
  return (
    <ol aria-label="Workspaces to make" className="space-y-1 px-1">
      {plan.map((item, i) => {
        const done = at === undefined ? undefined : started[i];
        return (
          <li
            key={i}
            style={{ animationDelay: `${i * 45}ms` }}
            className="flex min-w-0 items-center gap-2 text-xs text-neutral-500 motion-safe:animate-sheet-in motion-safe:[animation-fill-mode:both]"
          >
            <span aria-hidden="true" className="w-4 shrink-0 text-right font-mono text-neutral-600 tabular-nums">
              {i + 1}
            </span>
            <BranchIcon className="size-3.5 shrink-0" />
            <code aria-label="Branch" className="max-w-[45%] shrink-0 truncate rounded bg-emerald-400/10 px-1.5 py-0.5 font-mono text-emerald-300">
              {safeBranch(item.name)}
            </code>
            <span className="min-w-0 flex-1 truncate">{item.prompt.split("\n")[0]}</span>
            {done === true && (
              <span className="shrink-0 text-emerald-400" title="Started">
                ✓<span className="sr-only">started</span>
              </span>
            )}
            {done === false && (
              <span className="shrink-0 text-red-400" title="Did not start">
                ✕<span className="sr-only">did not start</span>
              </span>
            )}
            {done === undefined && at === i && (
              <span title="Starting" className="size-2 shrink-0 rounded-full bg-sky-400 motion-safe:animate-pulse">
                <span className="sr-only">starting</span>
              </span>
            )}
          </li>
        );
      })}
    </ol>
  );
}

/** One line forking into several: a task split into workspaces. */
function ForkIcon({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 16 16" className={className} fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M2 8h4M6 8l3-4.5h5M6 8h8M6 8l3 4.5h5" />
    </svg>
  );
}

/** A branch forking off: the branch a workspace is made on, in the task box and its plan. */
export function BranchIcon({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 16 16" className={className} fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
      <circle cx="4.5" cy="3.5" r="1.5" />
      <circle cx="4.5" cy="12.5" r="1.5" />
      <circle cx="11.5" cy="5.5" r="1.5" />
      <path d="M4.5 5v6M11.5 7c0 3-7 2-7 4" strokeLinecap="round" />
    </svg>
  );
}
