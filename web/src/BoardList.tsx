import { Link } from "@tanstack/react-router";
import { useId } from "react";
import { buttonShape, pressed } from "./Modal";
import { useCardChanges, useOnScreen } from "./cardChanges";
import { totals } from "./changes";
import type { Card } from "./lanes";
import { words } from "./labels";
import { Now } from "./Now";
import { glyphFill, glyphText, StateGlyph } from "./StateGlyph";
import { TerminalMenu } from "./TerminalMenu";
import { readStored, store } from "./storage";
import { ProjectFace } from "./ProjectFace";

/** Whether the board draws its workspaces as cards or as a list of one line each, kept for this browser. */
export type Layout = "cards" | "list";
const LAYOUT_KEY = "foundry.layout";
export function loadLayout(): Layout {
  return readStored(LAYOUT_KEY) === "list" ? "list" : "cards";
}
/** Keeps `layout` for next time; when not kept, the board still changes. */
export function saveLayout(layout: Layout) {
  store(LAYOUT_KEY, layout);
}

/** A line has room for this many terminals' glyphs; with more, it shows one fewer and says how many more. */
const GLYPHS_ROOM = 3;

/**
 * The switch between cards and the list, at the right of the board's grouping. The list fits a
 * fleet of dozens of workspaces on one screen, and many more on a phone than cards do.
 */
export function LayoutSwitch({ layout, onPick }: { layout: Layout; onPick: (layout: Layout) => void }) {
  return (
    <div role="group" aria-label="Show workspaces as" className="ml-auto flex shrink-0 items-center gap-1.5 text-xs">
      {(
        [
          ["cards", "Cards", "Each workspace as a card, with its terminals and what Claude said", <CardsIcon key="i" />],
          ["list", "List", "Each workspace on one line, to see a large fleet at once", <ListIcon key="i" />],
        ] as const
      ).map(([value, name, title, icon]) => (
        <button
          key={value}
          type="button"
          title={title}
          aria-pressed={layout === value}
          onClick={() => onPick(value)}
          className={`flex items-center gap-1.5 ${buttonShape} ${pressed} border-neutral-700 text-neutral-300 hover:bg-neutral-800`}
        >
          {icon}
          {name}
        </button>
      ))}
    </div>
  );
}

/**
 * A workspace on one line of the board's list: a glyph for each terminal, which opens it; the
 * workspace's name, which opens its most urgent terminal, over its project and branch; what that
 * terminal is doing (the question it asks, the tool it runs, or the first line Claude said); and
 * what the workspace changed, which opens the changes sheet. Its changes are read as a card's are.
 * On a phone, what the terminal is doing and the changes take a second line, so the name has the first.
 */
export function WorkspaceLine({ card, showMachine, onChanges, arrived = false }: { card: Card; showMachine: boolean; onChanges: (workspace: string) => void; arrived?: boolean }) {
  const id = useId();
  const [ref, onScreen] = useOnScreen<HTMLElement>();
  const { workspace, machine, rows, lead } = card;
  const changes = useCardChanges(card, onScreen);
  // A phone gives the line's second row to what its terminal is doing when there is nothing to count.
  const none = !changes.leaving && !changes.error && changes.data !== undefined && changes.data.files.length === 0 && changes.data.commits.length === 0;
  const shown = rows.slice(0, rows.length > GLYPHS_ROOM ? GLYPHS_ROOM - 1 : GLYPHS_ROOM);
  return (
    <article
      ref={ref}
      aria-labelledby={id}
      data-arrived={arrived || undefined}
      className={`relative grid grid-cols-[3rem_minmax(0,1fr)_auto] items-center gap-x-3 gap-y-1 py-2 pr-3 pl-4 transition-colors hover:bg-neutral-800/40 md:grid-cols-[4.5rem_minmax(0,16rem)_minmax(0,1fr)_auto] ${arrived ? "motion-safe:animate-arrive" : ""}`}
    >
      <span aria-hidden="true" className={`absolute inset-y-0 left-0 w-1 ${lead ? glyphFill[lead.glyph] : "bg-neutral-800"}`} />
      <span className="row-span-2 flex flex-wrap items-center self-start md:row-span-1 md:self-center">
        {shown.map((row) => (
          <TerminalMenu key={row.terminal} id={row.terminal}>
            <Link
              to="/terminals/$terminalId"
              params={{ terminalId: row.terminal }}
              aria-label={`${row.name} ${row.label}`}
              title={`${row.name}: ${row.label}${row.title ? ` · ${row.title}` : ""}`}
              className="grid size-6 place-items-center rounded hover:bg-neutral-800"
            >
              <StateGlyph glyph={row.glyph} />
            </Link>
          </TerminalMenu>
        ))}
        {rows.length > shown.length && <span className="w-6 text-center text-[11px] text-neutral-500">+{rows.length - shown.length}</span>}
      </span>
      <span className="col-span-2 flex min-w-0 flex-col md:col-span-1">
        <span className="flex min-w-0 items-baseline gap-2">
          <h3 id={id} className="min-w-0 truncate text-sm font-semibold text-neutral-100">
            {lead ? (
              <Link to="/terminals/$terminalId" params={{ terminalId: lead.terminal }} className="hover:underline hover:decoration-neutral-600 hover:underline-offset-4">
                {workspace.name}
              </Link>
            ) : (
              workspace.name
            )}
          </h3>
          {showMachine && (
            <span className="shrink-0 text-[11px] text-neutral-500" title={`Runs on ${machine.id}`}>
              {machine.name}
            </span>
          )}
        </span>
        <span className="flex min-w-0 items-center gap-1.5 font-mono text-[11px] text-neutral-500">
          <ProjectFace project={workspace.project} className="size-3.5 text-[7px]" />
          <span className="truncate">
            {workspace.project} · {workspace.branch}
          </span>
        </span>
      </span>
      <span className={`col-start-2 row-start-2 flex min-w-0 items-baseline gap-2 text-xs md:col-span-1 md:col-start-3 md:row-start-1 ${none ? "col-span-2" : ""}`}>
        {lead ? (
          <>
            <span className={`shrink-0 max-md:sr-only ${glyphText[lead.glyph]}`}>{lead.label}</span>
            <Doing row={lead} />
          </>
        ) : (
          <span className="text-neutral-500">No terminals</span>
        )}
      </span>
      <LineChanges name={workspace.name} changes={changes} none={none} onOpen={() => onChanges(workspace.id)} />
    </article>
  );
}

/** What the line's terminal is doing, in one line: its current step, or the first line of what Claude said. */
function Doing({ row }: { row: Card["rows"][number] }) {
  if (row.now) return <Now step={row.now} className="min-w-0 flex-1" />;
  if (row.said) {
    const first = row.said.split("\n").find((l) => l.trim()) ?? "";
    return (
      <span title={row.said} className="min-w-0 flex-1 truncate text-neutral-400">
        {first}
      </span>
    );
  }
  return null;
}

/** The line's changes, counted in a few characters; it opens the changes sheet. On a phone, nothing to count shows nothing. */
function LineChanges({ name, changes, none, onOpen }: { name: string; changes: ReturnType<typeof useCardChanges>; none: boolean; onOpen: () => void }) {
  const { data, error, leaving } = changes;
  let body: React.ReactNode;
  if (leaving) body = <span className="text-neutral-500">Archiving…</span>;
  else if (error) body = <span className="text-neutral-500">Not read</span>;
  else if (!data) body = <span className="text-neutral-600">…</span>;
  else if (none) body = <span className="text-neutral-500">No changes</span>;
  else {
    const t = totals(data);
    body = (
      <>
        <span className="font-mono">
          <span className="text-emerald-400">+{t.added}</span> <span className={t.removed ? "text-red-400" : "text-neutral-600"}>−{t.removed}</span>
        </span>
        {t.uncommitted > 0 && <span className="text-amber-300">{t.uncommitted} uncommitted</span>}
      </>
    );
  }
  return (
    <button
      type="button"
      onClick={onOpen}
      aria-label={`Changes in ${name}`}
      title={error ? words.unreadable : words.changes}
      className={`col-start-3 row-start-2 -my-1 flex shrink-0 items-center justify-end gap-2 justify-self-end rounded-md px-2 py-1 text-xs whitespace-nowrap hover:bg-neutral-800 md:col-start-4 md:row-start-1 md:my-0 ${none ? "max-md:hidden" : ""}`}
    >
      {body}
    </button>
  );
}

function CardsIcon() {
  return (
    <svg viewBox="0 0 16 16" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
      <rect x="2" y="2.5" width="5" height="5" rx="1" />
      <rect x="9" y="2.5" width="5" height="5" rx="1" />
      <rect x="2" y="9.5" width="5" height="4" rx="1" />
      <rect x="9" y="9.5" width="5" height="4" rx="1" />
    </svg>
  );
}

function ListIcon() {
  return (
    <svg viewBox="0 0 16 16" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" aria-hidden="true">
      <path d="M2.5 4h.01M2.5 8h.01M2.5 12h.01M5.5 4h8M5.5 8h8M5.5 12h8" />
    </svg>
  );
}
