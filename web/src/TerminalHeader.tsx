import { useQuery } from "@tanstack/react-query";
import { changesQuery } from "./ChangesPanel";
import { totals } from "./changes";
import type { Located } from "./fleet";
import { jumpKey, SearchIcon } from "./JumpPalette";
import { StatePill } from "./StateGlyph";

/**
 * The bar above the open terminal: which workspace and terminal it is, the machine it runs on and
 * its state, and the way into the workspace's changes. On a narrow screen it holds the button that
 * opens the sidebar as a drawer.
 */
export function TerminalHeader({
  located,
  drawerOpen,
  onMenu,
  onJump,
  onChanges,
}: {
  located?: Located;
  drawerOpen: boolean;
  onMenu: () => void;
  onJump: () => void;
  /** Opens the changes of the terminal's workspace. */
  onChanges: (workspace: string) => void;
}) {
  return (
    <header aria-label="Open terminal" className="flex h-11 shrink-0 items-center gap-3 border-b border-neutral-800 bg-neutral-900 px-3 text-sm">
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
            <span className="ml-2 hidden min-w-0 truncate text-xs text-neutral-500 sm:inline" title={`Runs on ${located.machine.name}`}>
              {located.machine.name}
            </span>
          </div>
          <StatePill glyph={located.glyph} label={located.label} />
          <ChangesButton workspace={located.workspace.id} onClick={() => onChanges(located.workspace.id)} />
        </>
      ) : (
        <span className="flex-1 text-neutral-500">Terminal</span>
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
