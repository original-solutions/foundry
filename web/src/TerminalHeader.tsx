import type { Located } from "./fleet";
import { jumpKey, SearchIcon } from "./JumpPalette";
import { StatePill } from "./StateGlyph";

/**
 * The bar above the open terminal: which workspace and terminal it is, the machine it runs on and
 * its state. On a narrow screen it holds the button that opens the sidebar as a drawer.
 */
export function TerminalHeader({ located, drawerOpen, onMenu, onJump }: { located?: Located; drawerOpen: boolean; onMenu: () => void; onJump: () => void }) {
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
