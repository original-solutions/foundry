import { Link } from "@tanstack/react-router";
import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { byUrgency, needingRows, rows, type MachineView, type Row, type WorkspaceView } from "./fleet";
import type { Glyph } from "./labels";
import { factoryBadge } from "./FactoryMark";
import { Named } from "./Now";
import { glyphText, StateGlyph } from "./StateGlyph";
import { TerminalMenu, useTerminalActions } from "./TerminalMenu";
import { readStored, store } from "./storage";

/** Whether the sessions list is docked beside the rail, kept in this browser only. */
const DOCK_KEY = "foundry.sessions";

export const loadDocked = () => readStored(DOCK_KEY) === "docked";
export const saveDocked = (on: boolean) => store(DOCK_KEY, on ? "docked" : "flyout");

/** The groups of the list, in the order they show: what needs a person first. */
const GROUPS = ["Needs you", "Working", "Ready", "Ended", "Unreachable"] as const;
type Group = (typeof GROUPS)[number];

const groupOf: Record<Glyph, Group> = {
  waiting: "Working",
  stuck: "Working",
  working: "Working",
  starting: "Working",
  ready: "Ready",
  done: "Ended",
  failed: "Ended",
  away: "Unreachable",
};

interface Item {
  row: Row;
  workspace: WorkspaceView;
  machine: string;
}

/**
 * Every terminal in the fleet, grouped by state. "Needs you" holds exactly what the board's
 * needs-you lane and the rail's count hold (`needingRows`); a factory run's quiet session that
 * does not need you sits with the working ones. Empty groups are left out.
 */
export function sessionGroups(machines: MachineView[]): { group: Group; items: Item[] }[] {
  const all = new Map<Group, Item[]>(GROUPS.map((g) => [g, []]));
  for (const machine of machines) {
    for (const workspace of machine.workspaces) {
      const needs = new Set(needingRows(workspace).map((r) => r.terminal));
      for (const row of byUrgency(rows(workspace))) {
        all.get(needs.has(row.terminal) ? "Needs you" : groupOf[row.glyph])!.push({ row, workspace, machine: machine.name });
      }
    }
  }
  return GROUPS.map((group) => ({ group, items: all.get(group)! })).filter((g) => g.items.length > 0);
}

interface Props {
  machines: MachineView[];
  selected?: string;
  docked: boolean;
  /** Whether the list takes the focus when it shows: when a person opened it, not when a docked list shows by itself. */
  focus: boolean;
  onDock: (on: boolean) => void;
  /** Called when a session is picked. */
  onPick: () => void;
}

/**
 * The fleet's sessions and shells as one list beside the rail: grouped by state, each row its
 * workspace, its factory issue, what it is doing and, with more than one machine, where. Opened by
 * a person, it focuses the open terminal's row, and the arrows walk it. Ended terminals clear from their group.
 */
export function SessionList({ machines, selected, docked, focus, onDock, onPick }: Props) {
  const groups = sessionGroups(machines);
  const many = machines.length > 1;
  const terminals = useTerminalActions();
  const ref = useRef<HTMLElement>(null);
  const count = groups.reduce((n, g) => n + g.items.length, 0);
  // One Tab stop, as the sidebar's list: the row last focused, else the open terminal's, else the first.
  const [focused, setFocused] = useState<string>();
  const ids = groups.flatMap((g) => g.items.map((i) => i.row.terminal));
  const stop = [focused, selected].find((id) => id !== undefined && ids.includes(id)) ?? ids[0];

  useEffect(() => {
    if (!focus) return;
    const links = [...(ref.current?.querySelectorAll<HTMLElement>("[data-session]") ?? [])];
    (links.find((l) => l.dataset.session === selected) ?? links[0])?.focus();
    // Only when the list opens: a fleet update must not move the focus.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const keys = (event: KeyboardEvent<HTMLElement>) => {
    const links = [...event.currentTarget.querySelectorAll<HTMLElement>("[data-session]")];
    const at = links.indexOf(event.target as HTMLElement);
    if (at === -1) return;
    const to = { ArrowDown: links[at + 1], ArrowUp: links[at - 1], Home: links[0], End: links.at(-1) }[event.key];
    if (!to) return;
    event.preventDefault();
    to.focus();
  };

  return (
    <section
      ref={ref}
      aria-label="Sessions"
      onKeyDown={keys}
      className={`flex h-full w-72 shrink-0 flex-col border-r border-neutral-800 bg-neutral-900 text-sm text-neutral-100 ${
        docked ? "" : "absolute inset-y-0 left-12 z-30 shadow-2xl shadow-black/50"
      }`}
    >
      <div className="flex shrink-0 items-center gap-2 px-3 pt-3 pb-2">
        <h2 className="font-semibold">Sessions</h2>
        <span className="text-xs text-neutral-500 tabular-nums">{count}</span>
        <button
          type="button"
          onClick={() => onDock(!docked)}
          title={docked ? "Float the list over the terminal, closing when you pick a session" : "Keep the list open beside the rail"}
          className="ml-auto rounded px-1 text-[11px] text-neutral-500 hover:bg-neutral-800 hover:text-neutral-200"
        >
          {docked ? "Undock" : "Dock beside the rail"}
        </button>
      </div>
      <div className="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto px-1.5 pb-3">
        {count === 0 && <p className="px-2 text-xs text-neutral-500">No sessions or shells yet. Start one with New task.</p>}
        {groups.map(({ group, items }) => {
          const ended = group === "Ended" ? items.map((i) => i.row.terminal) : [];
          return (
            <div key={group} role="group" aria-label={group}>
              <div className="flex items-center gap-1.5 px-2 pb-0.5 text-[10.5px] font-semibold tracking-wider text-neutral-500 uppercase">
                {group} <span className="tabular-nums">{items.length}</span>
                {terminals && ended.length > 0 && (
                  <button
                    type="button"
                    onClick={() => terminals.clear(ended)}
                    title="Clear the terminals that ended, with their final screens"
                    className="ml-auto rounded px-1 font-medium tracking-normal normal-case hover:bg-neutral-800 hover:text-neutral-200"
                  >
                    Clear {ended.length}
                  </button>
                )}
              </div>
              <ul>
                {items.map(({ row, workspace, machine }) => (
                  <li key={row.terminal}>
                    <SessionRow
                      row={row}
                      workspace={workspace}
                      machine={many ? machine : undefined}
                      current={row.terminal === selected}
                      stop={row.terminal === stop}
                      onFocus={() => setFocused(row.terminal)}
                      onPick={onPick}
                    />
                  </li>
                ))}
              </ul>
            </div>
          );
        })}
      </div>
    </section>
  );
}

function SessionRow({
  row,
  workspace,
  machine,
  current,
  stop,
  onFocus,
  onPick,
}: {
  row: Row;
  workspace: WorkspaceView;
  machine?: string;
  current: boolean;
  /** Whether Tab stops here. */
  stop: boolean;
  onFocus: () => void;
  onPick: () => void;
}) {
  const name = `${workspace.name} / ${row.name}: ${row.label}`;
  const quiet = row.glyph === "done" || row.glyph === "away";
  // No place for the menu: its Rename opens the terminal and edits the name in its tab, since the sidebar's row is not shown beside the rail.
  return (
    <TerminalMenu id={row.terminal}>
      <Link
        to="/terminals/$terminalId"
        params={{ terminalId: row.terminal }}
        aria-label={name}
        aria-current={current ? "true" : undefined}
        title={row.title ? `${name}\n${row.title}` : name}
        data-session={row.terminal}
        tabIndex={stop ? 0 : -1}
        onFocus={onFocus}
        onClick={onPick}
        className={`grid grid-cols-[0.75rem_minmax(0,1fr)_auto] items-center gap-x-2 rounded-md px-2 py-1.5 hover:bg-neutral-800 focus-visible:outline-2 focus-visible:outline-emerald-400 ${
          current ? "bg-neutral-800 shadow-[inset_2px_0_0] shadow-emerald-400" : ""
        }`}
      >
        <StateGlyph glyph={row.glyph} />
        <span className={`truncate font-medium ${quiet ? "text-neutral-400" : ""}`}>{workspace.name}</span>
        {/* The issue as text: the row is already a link, and FactoryMark is one too. */}
        {workspace.factory?.issue !== undefined ? (
          <span className={factoryBadge}>#{workspace.factory.issue}</span>
        ) : (
          <span />
        )}
        <span className="col-start-2 col-end-4 flex min-w-0 items-baseline gap-1.5 text-xs text-neutral-400">
          <span className={`shrink-0 ${quiet ? "" : glyphText[row.glyph]}`}>{row.label}</span>
          <span className="text-neutral-600">·</span>
          <Named name={row.name} title={row.title} step={row.now} />
          {machine && <span className="shrink-0 text-neutral-500">{machine}</span>}
        </span>
      </Link>
    </TerminalMenu>
  );
}
