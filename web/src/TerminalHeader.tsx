import { useQuery } from "@tanstack/react-query";
import { changesQuery } from "./ChangesPanel";
import { findKey } from "./FindBar";
import { totals } from "./changes";
import type { Located } from "./fleet";
import { jumpKey, SearchIcon } from "./JumpPalette";
import { StatePill } from "./StateGlyph";
import { StepsIcon } from "./StepsPanel";

/** The key that opens a terminal beside the open one, or closes the split. */
export const splitKey = jumpKey === "⌘K" ? "⌘\\" : "Ctrl \\";

/**
 * The bar above a terminal: which workspace and terminal it is, the machine it runs on and its
 * state, and the ways into the workspace's changes, the session's steps and finding text in it. On a narrow screen it holds the button that
 * opens the sidebar as a drawer. On a wide one it opens another terminal beside this one, or, in
 * a split, closes its own side. In a split the side with the keyboard has a lit edge; the other
 * is dimmed.
 */
export function TerminalHeader({
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
}: {
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
}) {
  return (
    <header
      aria-label={beside ? "Terminal beside" : "Open terminal"}
      className={`relative h-11 min-w-0 flex-1 items-center gap-3 border-b border-neutral-800 bg-neutral-900 px-3 text-sm ${beside ? "hidden opacity-60 transition-opacity hover:opacity-100 md:flex" : "flex"}`}
    >
      {onClosePane && !beside && <span aria-hidden="true" className="absolute inset-x-0 bottom-0 h-0.5 bg-sky-400/70" />}
      <button
        type="button"
        aria-label="Workspaces"
        aria-controls="sidebar"
        aria-expanded={drawerOpen}
        onClick={onMenu}
        className="-ml-1 rounded p-1.5 text-neutral-300 hover:bg-neutral-800 md:hidden"
      >
        <svg viewBox="0 0 16 16" className="size-4" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
          <path d="M2 4h12M2 8h12M2 12h12" strokeLinecap="round" />
        </svg>
      </button>
      {located ? (
        <>
          <div className="flex min-w-0 flex-1 items-baseline gap-1.5">
            <span className="truncate font-medium text-neutral-100">{located.workspace.name}</span>
            <span className="text-neutral-600">/</span>
            <span className="shrink-0 text-neutral-300">{located.name}</span>
            {located.title && (
              <span className="hidden min-w-0 truncate text-neutral-400 sm:inline" title={located.title}>
                · {located.title}
              </span>
            )}
            <span className="ml-2 hidden min-w-0 truncate text-xs text-neutral-500 sm:inline" title={`Runs on ${located.machine.name}`}>
              {located.machine.name}
            </span>
          </div>
          <StatePill glyph={located.glyph} label={located.label} />
          <ChangesButton workspace={located.workspace.id} onClick={() => onChanges(located.workspace.id)} />
          {onSteps && (
            <button
              type="button"
              aria-label="Steps"
              aria-pressed={stepsOpen}
              title="Every prompt, tool and question of this session, turn by turn"
              onClick={onSteps}
              className={`flex shrink-0 items-center gap-1.5 rounded border px-2 py-0.5 text-xs ${stepsOpen ? "border-sky-400/50 bg-sky-400/10 text-sky-200" : "border-neutral-700 text-neutral-300 hover:border-neutral-500 hover:text-neutral-100"}`}
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
              className={`shrink-0 rounded p-1.5 ${finding ? "bg-sky-400/10 text-sky-200" : "text-neutral-400 hover:bg-neutral-800 hover:text-neutral-200"}`}
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
      <button
        type="button"
        aria-label="Jump to"
        title={`Jump to (${jumpKey})`}
        onClick={onJump}
        className="-mr-1 rounded p-1.5 text-neutral-400 hover:bg-neutral-800 hover:text-neutral-200"
      >
        <SearchIcon className="size-4" />
      </button>
    </header>
  );
}

/** How often the header reads a workspace's changes again, once they have been opened. */
const HEADER_EVERY = 15_000;

/**
 * Opens the workspace's changes. Once they have been opened, it shows their size and keeps it
 * fresh; until then it reads nothing, so a header costs the daemon nothing.
 */
function ChangesButton({ workspace, onClick }: { workspace: string; onClick: () => void }) {
  const { data } = useQuery({ ...changesQuery(workspace), enabled: (query) => query.state.data !== undefined, refetchInterval: HEADER_EVERY, retry: false });
  const t = data && totals(data);
  return (
    <button
      type="button"
      onClick={onClick}
      title="Changes since the base branch"
      className="flex shrink-0 items-center gap-1.5 rounded border border-neutral-700 px-2 py-0.5 text-xs text-neutral-300 hover:border-neutral-500 hover:text-neutral-100"
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
