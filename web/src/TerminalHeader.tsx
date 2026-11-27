import { useQuery } from "@tanstack/react-query";
import { useRef } from "react";
import { useLeaving } from "./Archive";
import { changesQuery } from "./ChangesPanel";
import { findKey } from "./FindBar";
import { totals } from "./changes";
import type { Located } from "./fleet";
import { machineQuery } from "./api";
import { useWidth } from "./media";
import { jumpKey, SearchIcon } from "./JumpPalette";
import { button, pressed } from "./Modal";
import { NextSteps, offersNext } from "./NextSteps";
import { NextUp } from "./NextUp";
import type { Needing } from "./needing";
import { needsYou, words } from "./labels";
import { reply } from "./reply";
import { StepsIcon } from "./StepsPanel";
import { TabStrip } from "./TabStrip";
import { TerminalMenuButton } from "./TerminalMenu";
import { ProjectFace } from "./ProjectFace";

/** The key that opens a terminal beside the open one, or closes the split. */
export const splitKey = jumpKey === "⌘K" ? "⌘\\" : "Ctrl \\";

/**
 * How much the header holds at its width. Wide holds everything. Snug gives up the next steps,
 * the machine's name and the next one's workspace, and its buttons keep their icons. Tight folds
 * changes and steps into More (which holds both), keeps Next to its count and puts two or more
 * tabs on their own row, so the workspace's name and the open tab keep the room. Wide until the
 * header has been measured.
 */
export type Fit = "wide" | "snug" | "tight";

export function headerFit(width: number | undefined): Fit {
  if (width === undefined || width >= WIDE) return "wide";
  return width >= SNUG ? "snug" : "tight";
}

/** The header's widths, in pixels, from which it is wide or snug. */
const WIDE = 1120;
const SNUG = 640;

/**
 * The one bar above a terminal: its workspace and the machine it runs on, then the workspace's
 * terminals as tabs (the open one with its state), and the ways into the workspace's changes, the
 * session's steps, finding text in it and a menu with the rest, ending it included. On a narrow
 * screen it holds the button that opens the sidebar as a drawer, and the way to jump that the
 * sidebar holds on a wide one. On a wide one it opens another terminal beside this one, or, in a
 * split, closes its own side. In a split each side has its bar; the side with the keyboard has a
 * lit edge and the other is dimmed. An idle session of this machine offers its workspace's next
 * steps (commit, open a pull request, fix failing checks) in the bar, each sent to it as a prompt;
 * with a mouse, outside a split and while the bar is wide only (a phone's key bar offers them).
 * When another terminal needs you, the bar of the side with the keyboard leads the way to it
 * (`NextUp`). The bar never spills out of its box: as it narrows, what matters least gives way
 * first (`headerFit`).
 */
export function TerminalHeader({
  id,
  located,
  drawerOpen,
  onMenu,
  onJump,
  onChanges,
  onSteps,
  stepsOpen = false,
  onFind,
  finding = false,
  onSplit,
  onClosePane,
  beside = false,
  changed = none,
  upNext,
}: {
  /** The terminal it heads, for its menu. */
  id?: string;
  located?: Located;
  drawerOpen: boolean;
  onMenu: () => void;
  onJump: () => void;
  /** Opens the changes of the terminal's workspace. */
  onChanges: (workspace: string) => void;
  /** Shows or hides the steps column, which follows the terminal with the keyboard. */
  onSteps?: () => void;
  stepsOpen?: boolean;
  /** Shows or hides the find bar over the terminal with the keyboard. */
  onFind?: () => void;
  finding?: boolean;
  /** Offered when nothing is beside: picks a terminal to show beside this one. */
  onSplit?: () => void;
  /** Offered in a split: closes this side, leaving the other. */
  onClosePane?: () => void;
  /** This is the side of a split without the keyboard: dimmed, and hidden on a narrow screen. */
  beside?: boolean;
  /** Terminals whose state changed in the background since they were last shown. */
  changed?: Set<string>;
  /** The next terminal that needs you, and how many do besides this one. */
  upNext?: { next: Needing; others: number };
}) {
  const { data: here } = useQuery(machineQuery);
  const bar = useRef<HTMLElement>(null);
  const fit = headerFit(useWidth(bar));
  // Two or more tabs take a row of their own: in a half of a split, and in a tight bar.
  const apart = !!onClosePane || fit === "tight";
  return (
    <header
      ref={bar}
      aria-label={beside ? "Terminal beside" : "Open terminal"}
      className={`relative min-h-11 min-w-0 flex-1 flex-wrap items-center ${fit === "tight" ? "gap-x-2" : "gap-x-3"} border-b border-neutral-800 bg-neutral-900 px-3 text-sm ${apart ? "" : "md:flex-nowrap"} ${beside ? "hidden opacity-60 transition-opacity hover:opacity-100 md:flex" : "flex"}`}
    >
      {onClosePane && !beside && <span aria-hidden="true" className="absolute inset-x-0 bottom-0 h-0.5 bg-sky-400/70" />}
      <button
        type="button"
        aria-label="Workspaces"
        aria-controls="sidebar"
        aria-expanded={drawerOpen}
        onClick={onMenu}
        className="-ml-1 shrink-0 rounded p-1.5 text-neutral-300 hover:bg-neutral-800 md:hidden"
      >
        <svg viewBox="0 0 16 16" className="size-4" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
          <path d="M2 4h12M2 8h12M2 12h12" strokeLinecap="round" />
        </svg>
      </button>
      {located ? (
        <>
          {/* It shares the room with the tabs beside it (see `TabStrip`), so neither squeezes the other out. */}
          <div className={`flex h-11 min-w-0 flex-1 items-center gap-2 ${apart ? "" : "md:max-w-64 md:grow-0 md:basis-auto"}`}>
            {/* On a phone the bar is short of room, and the workspace's name matters more than its project's face. */}
            <ProjectFace project={located.workspace.project} className="size-4 text-[8px] max-md:hidden" />
            <span className="truncate font-medium text-neutral-100" title={`${located.workspace.branch} · ${located.workspace.project}`}>
              {located.workspace.name}
            </span>
            {fit === "wide" && (
              <span className="shrink-0 text-xs text-neutral-500" title={`Runs on ${located.machine.name}`}>
                {located.machine.name}
              </span>
            )}
          </div>
          {id ? <TabStrip workspace={located.workspace} machine={located.machine} active={id} changed={changed} apart={apart} /> : <span className="flex-1" />}
          {upNext && !beside && <NextUp next={upNext.next} others={upNext.others} answered={!needsYou.includes(located.glyph)} fit={fit} />}
          {id && !onClosePane && fit === "wide" && offersNext(located, here) && (
            <div className="hidden pointer-fine:contents">
              <NextSteps workspace={located.workspace.id} name={located.name} send={(prompt) => reply(id, prompt)} look="header" />
            </div>
          )}
          {fit !== "tight" && <ChangesButton workspace={located.workspace.id} onClick={() => onChanges(located.workspace.id)} worded={fit === "wide"} />}
          {onSteps && fit !== "tight" && (
            <button
              type="button"
              aria-label="Steps"
              aria-pressed={stepsOpen}
              title="Every prompt, tool and question of this session, turn by turn"
              onClick={onSteps}
              className={`flex shrink-0 items-center gap-1.5 ${button} ${pressed}`}
            >
              <StepsIcon className="size-3.5" />
              <span className={fit === "wide" ? undefined : "sr-only"}>Steps</span>
            </button>
          )}
          {onFind && (
            <button
              type="button"
              aria-label="Find"
              aria-pressed={finding}
              title={`Find in this terminal's scrollback (${findKey})`}
              onClick={onFind}
              className={`hidden shrink-0 rounded p-1.5 md:block ${finding ? "bg-sky-400/10 text-sky-200" : "text-neutral-400 hover:bg-neutral-800 hover:text-neutral-200"}`}
            >
              <FindIcon className="size-4" />
            </button>
          )}
        </>
      ) : (
        <span className="flex-1 text-neutral-500">Terminal</span>
      )}
      {onSplit && (
        <button
          type="button"
          aria-label="Open beside"
          title={`Open another terminal beside this one (${splitKey})`}
          onClick={onSplit}
          className="hidden rounded p-1.5 text-neutral-400 hover:bg-neutral-800 hover:text-neutral-200 md:block"
        >
          <svg viewBox="0 0 16 16" className="size-4" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
            <rect x="2" y="2.5" width="12" height="11" rx="1.5" />
            <path d="M8 2.5v11" />
          </svg>
        </button>
      )}
      {onClosePane && (
        <button
          type="button"
          aria-label="Close pane"
          title="Close this side (the terminal keeps running)"
          onClick={onClosePane}
          className="hidden rounded p-1.5 text-neutral-400 hover:bg-neutral-800 hover:text-neutral-200 md:block"
        >
          <svg viewBox="0 0 16 16" className="size-4" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
            <rect x="2" y="2.5" width="12" height="11" rx="1.5" />
            <path d="M6 6l4 4M10 6l-4 4" strokeLinecap="round" />
          </svg>
        </button>
      )}
      {id && <TerminalMenuButton id={id} />}
      <button
        type="button"
        aria-label="Jump to"
        title={`Jump to (${jumpKey})`}
        onClick={onJump}
        className="-mr-1 shrink-0 rounded p-1.5 text-neutral-400 hover:bg-neutral-800 hover:text-neutral-200 md:hidden"
      >
        <SearchIcon className="size-4" />
      </button>
    </header>
  );
}

const none = new Set<string>();

/** How often the header reads a workspace's changes again, once they have been opened. */
const HEADER_EVERY = 15_000;

/**
 * Opens the workspace's changes. Once they have been opened, it shows their size and keeps it
 * fresh; until then it reads nothing, so a header costs the daemon nothing. Its word shows when
 * `worded`; it is its name either way.
 */
function ChangesButton({ workspace, onClick, worded }: { workspace: string; onClick: () => void; worded: boolean }) {
  const leaving = useLeaving(workspace);
  const { data } = useQuery({ ...changesQuery(workspace), enabled: (query) => !leaving && query.state.data !== undefined, refetchInterval: HEADER_EVERY, retry: false });
  const t = data && !leaving && totals(data);
  return (
    <button
      type="button"
      onClick={onClick}
      title={words.changes}
      className={`flex shrink-0 items-center gap-1.5 ${button}`}
    >
      <svg viewBox="0 0 16 16" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
        <path d="M8 2.5v6M5 5.5h6M5 12.5h6" strokeLinecap="round" />
      </svg>
      <span className={worded ? undefined : "sr-only"}>Changes</span>
      {t && t.files > 0 && (
        <span className="font-mono">
          <span className="text-emerald-400">+{t.added}</span> <span className={t.removed ? "text-red-400" : "text-neutral-600"}>−{t.removed}</span>
        </span>
      )}
    </button>
  );
}

/** Lines of text with a magnifier: find in the terminal's text, unlike the jump palette's plain magnifier. */
function FindIcon({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 16 16" className={className} fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
      <path d="M2 3.5h10M2 7h5M2 10.5h3.5" strokeLinecap="round" />
      <circle cx="10.5" cy="10" r="2.75" />
      <path d="M12.5 12l1.75 1.75" strokeLinecap="round" />
    </svg>
  );
}
