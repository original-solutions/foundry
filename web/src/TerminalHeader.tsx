import { useQuery } from "@tanstack/react-query";
import { useLeaving } from "./Archive";
import { changesQuery } from "./ChangesPanel";
import { findKey } from "./FindBar";
import { totals } from "./changes";
import type { Located } from "./fleet";
import { machineQuery } from "./api";
import { words } from "./labels";
import { jumpKey, SearchIcon } from "./JumpPalette";
import { button, pressed } from "./Modal";
import { NextSteps, offersNext } from "./NextSteps";
import { reply } from "./reply";
import { StepsIcon } from "./StepsPanel";
import { TabStrip } from "./TabStrip";
import { TerminalMenuButton } from "./TerminalMenu";

/** The key that opens a terminal beside the open one, or closes the split. */
export const splitKey = jumpKey === "⌘K" ? "⌘\\" : "Ctrl \\";

/**
 * The one bar above a terminal: its workspace and the machine it runs on, then the workspace's
 * terminals as tabs (the open one with its state), and the ways into the workspace's changes, the
 * session's steps, finding text in it and a menu with the rest, ending it included. On a narrow
 * screen it holds the button that opens the sidebar as a drawer, and the way to jump that the
 * sidebar holds on a wide one. On a wide one it opens another terminal beside this one, or, in a
 * split, closes its own side. In a split each side has its bar; the side with the keyboard has a
 * lit edge and the other is dimmed. An idle session of this machine offers its workspace's next
 * steps (commit, open a pull request, fix failing checks) in the bar, each sent to it as a prompt;
 * with a mouse on a wide screen and outside a split only, where the bar has room (a phone's key
 * bar offers them).
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
}) {
  const { data: here } = useQuery(machineQuery);
  return (
    <header
      aria-label={beside ? "Terminal beside" : "Open terminal"}
      className={`relative min-h-11 min-w-0 flex-1 flex-wrap items-center gap-x-3 border-b border-neutral-800 bg-neutral-900 px-3 text-sm ${onClosePane ? "" : "md:flex-nowrap"} ${beside ? "hidden opacity-60 transition-opacity hover:opacity-100 md:flex" : "flex"}`}
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
          <div className={`flex h-11 min-w-0 shrink items-center gap-2 ${onClosePane ? "flex-1" : "max-md:flex-1 md:max-w-64"}`}>
            <span className="truncate font-medium text-neutral-100" title={`${located.workspace.branch} · ${located.workspace.project}`}>
              {located.workspace.name}
            </span>
            <span className="hidden shrink-0 text-xs text-neutral-500 sm:inline" title={`Runs on ${located.machine.name}`}>
              {located.machine.name}
            </span>
          </div>
          {id ? <TabStrip workspace={located.workspace} machine={located.machine} active={id} changed={changed} apart={!!onClosePane} /> : <span className="flex-1" />}
          {id && !onClosePane && offersNext(located, here) && (
            <div className="hidden xl:pointer-fine:contents">
              <NextSteps workspace={located.workspace.id} name={located.name} send={(prompt) => reply(id, prompt)} look="header" />
            </div>
          )}
          <ChangesButton workspace={located.workspace.id} onClick={() => onChanges(located.workspace.id)} />
          {onSteps && (
            <button
              type="button"
              aria-label="Steps"
              aria-pressed={stepsOpen}
              title="Every prompt, tool and question of this session, turn by turn"
              onClick={onSteps}
              className={`flex shrink-0 items-center gap-1.5 ${button} ${pressed}`}
            >
              <StepsIcon className="size-3.5" />
              <span className="sr-only sm:not-sr-only">Steps</span>
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
 * fresh; until then it reads nothing, so a header costs the daemon nothing.
 */
function ChangesButton({ workspace, onClick }: { workspace: string; onClick: () => void }) {
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
      <span className="sr-only sm:not-sr-only">Changes</span>
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
