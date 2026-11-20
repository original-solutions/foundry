import { useMutation } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useState, type ReactNode } from "react";
import { ArchiveButton } from "./ArchiveButton";
import { button } from "./Modal";
import { jumpKey, SearchIcon } from "./JumpPalette";
import { Named, Now } from "./Now";
import { startShell } from "./api";
import { loadFolded, saveFolded } from "./fold";
import { byUrgency, machineNumbers, machineStats, rows, type MachineView, type Row, type WorkspaceView } from "./fleet";
import { glyphText, StateGlyph } from "./StateGlyph";
import { ended } from "./ends";
import { TerminalMenu, useTerminalActions } from "./TerminalMenu";
import { Mark } from "./Mark";
import { ThemeButton } from "./ThemeButton";
import type { MachineSample } from "./api";

const statusColour = { online: "text-emerald-400", offline: "text-red-400", unknown: "text-neutral-400" };
const statusDot = { online: "bg-emerald-400", offline: "bg-red-400", unknown: "bg-neutral-500" };

/** How many states a folded workspace shows on its line before it counts the rest. */
const STRIP = 6;

interface Props {
  machines: MachineView[];
  /** Opens the new-workspace dialog. */
  onCreate: () => void;
  /** The terminal open in the main pane. */
  selected?: string;
  /** Called when a terminal is picked, so a drawer can close. */
  onNavigate?: () => void;
  /** Given while the sidebar is a drawer over the terminal: its close button. */
  onClose?: () => void;
  /** Opens the jump palette. */
  onJump?: () => void;
  /** Opens a workspace's changes. */
  onChanges?: (workspace: string) => void;
  /** Shown first among the header's buttons: the alerts bell. */
  alerts?: ReactNode;
}

export function Sidebar({ machines, onCreate, selected, onNavigate, onClose, onJump, onChanges, alerts }: Props) {
  const [folded, setFolded] = useState(() => new Set(loadFolded()));
  const fold = (ids: string[], on: boolean) =>
    setFolded((old) => {
      const next = new Set(old);
      for (const id of ids) {
        if (on) next.add(id);
        else next.delete(id);
      }
      // Keep only workspaces still listed, so archived ones do not pile up in storage.
      const listed = new Set(machines.flatMap((m) => m.workspaces.map((w) => w.id)));
      saveFolded([...next].filter((id) => listed.has(id)));
      return next;
    });

  return (
    <nav id="sidebar" className="flex h-full w-full shrink-0 flex-col gap-4 overflow-y-auto bg-neutral-900 p-3 text-sm text-neutral-100 md:w-80">
      <div className="flex items-center justify-between gap-2">
        <h1 className="flex items-center gap-1.5 text-lg font-semibold">
          <Mark className="size-3.5 text-emerald-400" />
          Foundry
        </h1>
        <div className="flex items-center gap-1.5">
          <ThemeButton />
          {alerts}
          {onJump && (
            <button type="button" aria-label="Jump to" title={`Jump to (${jumpKey})`} onClick={onJump} className="rounded p-1 text-neutral-400 hover:bg-neutral-800 hover:text-neutral-200">
              <SearchIcon className="size-4" />
            </button>
          )}
          <button type="button" onClick={onCreate} className={`${button} whitespace-nowrap`}>
            New workspace
          </button>
          {onClose && (
            <button type="button" aria-label="Close workspaces" onClick={onClose} className="rounded p-1 text-neutral-300 hover:bg-neutral-800 md:hidden">
              <svg viewBox="0 0 16 16" className="size-4" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
                <path d="M4 4l8 8M12 4l-8 8" strokeLinecap="round" />
              </svg>
            </button>
          )}
        </div>
      </div>
      {machines.map((machine) => {
        const ids = machine.workspaces.map((w) => w.id);
        const allFolded = ids.length > 0 && ids.every((id) => folded.has(id));
        return (
          <section key={machine.id} aria-label={machine.name} className="space-y-2">
            <div className="px-1">
              <div className="flex items-center gap-2">
                <span aria-hidden="true" className={`size-1.5 shrink-0 rounded-full ${statusDot[machine.status]}`} />
                <h2 className="min-w-0 truncate font-semibold">
                  <Link
                    to="/machines"
                    onClick={onNavigate}
                    title={`${machine.id}: its load, memory, disk and workspaces`}
                    className="rounded hover:underline hover:decoration-neutral-600 hover:underline-offset-4"
                  >
                    {machine.name}
                  </Link>
                </h2>
                <span className={`text-xs ${statusColour[machine.status]}`}>{machine.status}</span>
                {ids.length > 0 && (
                  <button
                    type="button"
                    onClick={() => fold(ids, !allFolded)}
                    className="ml-auto shrink-0 rounded px-1 text-[11px] text-neutral-500 hover:bg-neutral-800 hover:text-neutral-200"
                  >
                    {allFolded ? "Unfold all" : "Fold all"}
                  </button>
                )}
              </div>
              <div className="mt-1 flex items-start gap-2 text-xs text-neutral-400">
                {machine.sample && <LoadMeter sample={machine.sample} />}
                <span className="min-w-0" title={machineNumbers(machine.sample)}>
                  {machineStats(machine.sample)}
                </span>
              </div>
            </div>
            <div className="space-y-1">
              {machine.workspaces.map((workspace) => (
                <Workspace
                  key={workspace.id}
                  workspace={workspace}
                  machine={machine.id}
                  folded={folded.has(workspace.id)}
                  onFold={(on) => fold([workspace.id], on)}
                  selected={selected}
                  onNavigate={onNavigate}
                  onChanges={onChanges}
                />
              ))}
            </div>
          </section>
        );
      })}
    </nav>
  );
}

/** Load per core as a short bar: green with room, amber near full, red past one per core. */
function LoadMeter({ sample }: { sample: MachineSample }) {
  const load = sample.load_per_core;
  const tone = load >= 1 ? "bg-red-400" : load >= 0.7 ? "bg-amber-400" : "bg-emerald-400";
  return (
    <span
      role="meter"
      aria-label="Load per core"
      aria-valuenow={load}
      aria-valuemin={0}
      aria-valuemax={Math.max(1, load)}
      className="mt-[5px] h-1.5 w-8 shrink-0 overflow-hidden rounded-full bg-neutral-800"
    >
      <span className={`block h-full rounded-full ${tone}`} style={{ width: `${Math.min(load, 1) * 100}%` }} />
    </span>
  );
}

interface WorkspaceProps {
  workspace: WorkspaceView;
  /** The machine it is on. */
  machine: string;
  folded: boolean;
  onFold: (on: boolean) => void;
  selected?: string;
  onNavigate?: () => void;
  onChanges?: (workspace: string) => void;
}

function Workspace({ workspace, machine, folded, onFold, selected, onNavigate, onChanges }: WorkspaceProps) {
  const shell = useMutation({ mutationFn: () => startShell(workspace.id) });
  const terminals = useTerminalActions();
  const list = rows(workspace);
  // Only the machine serving the UI can clear its terminals.
  const over = terminals?.here === machine ? ended(workspace) : [];
  return (
    <div role="group" aria-label={workspace.name} className="group/ws">
      <div className="flex items-center gap-2 rounded-md px-1 py-1 hover:bg-neutral-800/60">
        <button
          type="button"
          aria-expanded={!folded}
          onClick={() => onFold(!folded)}
          title={folded ? "Unfold" : "Fold"}
          className="flex min-w-0 flex-1 items-center gap-1.5 text-left"
        >
          <svg viewBox="0 0 16 16" aria-hidden="true" className={`size-3 shrink-0 text-neutral-500 transition-transform motion-reduce:transition-none ${folded ? "" : "rotate-90"}`} fill="currentColor">
            <path d="M6 3.5l5 4.5-5 4.5z" />
          </svg>
          <span className="truncate font-medium">{workspace.name}</span>
        </button>
        {folded && <Strip list={list} selected={selected} onNavigate={onNavigate} />}
      </div>
      {!folded && (
        <>
          <div className="flex items-center gap-2 pl-6 pr-1">
            <span className="min-w-0 flex-1 truncate text-xs text-neutral-500">
              {workspace.branch} · {workspace.project}
            </span>
            <div className="flex shrink-0 items-center gap-0.5 pointer-fine:hidden pointer-fine:group-focus-within/ws:flex pointer-fine:group-hover/ws:flex">
              {onChanges && (
                <Action label="Changes" title="What this workspace changed since its base" onClick={() => onChanges(workspace.id)}>
                  <path d="M8 2.5v6M5 5.5h6M5 12.5h6" />
                </Action>
              )}
              {terminals && (
                <Action label="New session" title="New Claude Code session" onClick={() => terminals.onNewSession(workspace.id, workspace.name)}>
                  <path d="M8 2.5l1.3 3.2 3.2 1.3-3.2 1.3L8 11.5 6.7 8.3 3.5 7l3.2-1.3z" />
                </Action>
              )}
              <Action label="New shell" title="New shell" onClick={() => shell.mutate()} disabled={shell.isPending}>
                <path d="M3.5 4.5l3 3-3 3M8 11.5h4.5" />
              </Action>
              {terminals && over.length > 0 && (
                <Action label={`Clear ${over.length} ended`} title="Clear the terminals that ended, with their final screens" onClick={() => terminals.clear(over)}>
                  <path d="M11.5 2l-3 6M4 8h7l1 5.5H3zM6 10.5v3M9 10.5v3" />
                </Action>
              )}
              <ArchiveButton
                workspace={workspace.id}
                name={workspace.name}
                trigger={
                  <Action label="Archive" title="Archive: stop its terminals, remove the worktree, keep the branch">
                    <path d="M2.5 3.5h11v3h-11zM3.5 6.5v6h9v-6M6.5 9h3" />
                  </Action>
                }
              />
            </div>
          </div>
          {shell.error && (
            <p role="alert" className="pl-6 text-xs text-red-400">
              {shell.error.message}
            </p>
          )}
          {list.length > 0 && (
            <ul className="ml-[11px] mt-0.5 border-l border-neutral-800 pl-1.5">
              {list.map((row) => (
                <Item key={row.terminal} row={row} selected={selected} onNavigate={onNavigate} />
              ))}
            </ul>
          )}
        </>
      )}
    </div>
  );
}

/**
 * A folded workspace's states on its own line, the most urgent first, each a link to its
 * terminal. The open terminal always keeps a place, so the sidebar still shows where you are.
 */
function Strip({ list, selected, onNavigate }: { list: Row[]; selected?: string; onNavigate?: () => void }) {
  const ordered = byUrgency(list);
  let shown = ordered.slice(0, STRIP);
  const open = ordered.slice(STRIP).find((r) => r.terminal === selected);
  if (open) shown = [...shown.slice(0, STRIP - 1), open];
  const rest = list.length - shown.length;
  return (
    <span className="flex shrink-0 items-center gap-0.5">
      {shown.map((row) => (
        <TerminalMenu key={row.terminal} id={row.terminal}>
          <Link
            to="/terminals/$terminalId"
            params={{ terminalId: row.terminal }}
            aria-label={`${row.name} ${row.label}`}
            title={`${row.name}: ${row.label}`}
            aria-current={row.terminal === selected ? "true" : undefined}
            onClick={onNavigate}
            className={`grid size-5 place-items-center rounded hover:bg-neutral-700 ${row.terminal === selected ? "bg-neutral-700" : ""}`}
          >
            <StateGlyph glyph={row.glyph} />
          </Link>
        </TerminalMenu>
      ))}
      {rest > 0 && <span className="pl-0.5 text-[11px] text-neutral-500">+{rest}</span>}
    </span>
  );
}

function Action({ label, title, onClick, disabled, children }: { label: string; title: string; onClick?: () => void; disabled?: boolean; children: ReactNode }) {
  return (
    <button
      type="button"
      aria-label={label}
      title={title}
      onClick={onClick}
      disabled={disabled}
      className="grid size-6 place-items-center rounded text-neutral-400 hover:bg-neutral-700 hover:text-neutral-100 disabled:opacity-40"
    >
      <svg viewBox="0 0 16 16" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
        {children}
      </svg>
    </button>
  );
}

function Item({ row, selected, onNavigate }: { row: Row; selected?: string; onNavigate?: () => void }) {
  const current = row.terminal === selected;
  const ended = row.glyph === "done" || row.glyph === "away";
  return (
    <li>
      <TerminalMenu id={row.terminal}>
        <Link
          to="/terminals/$terminalId"
          params={{ terminalId: row.terminal }}
          aria-current={current ? "true" : undefined}
          onClick={onNavigate}
          title={row.title ? `${row.name}: ${row.title}` : undefined}
          className={`flex items-start gap-2 rounded px-2 py-1.5 hover:bg-neutral-800 md:py-1 ${current ? "bg-neutral-800 text-neutral-50" : ended ? "text-neutral-500" : ""}`}
        >
          <span className="flex h-5 shrink-0 items-center">
            <StateGlyph glyph={row.glyph} />
          </span>
          <span className="min-w-0 flex-1">
            <span className="flex items-baseline gap-2 leading-5">
              <Named name={row.name} title={row.title} /> <span className={`shrink-0 text-xs ${ended ? "" : glyphText[row.glyph]}`}>{row.label}</span>
            </span>
            {row.now && <Now step={row.now} />}
          </span>
        </Link>
      </TerminalMenu>
    </li>
  );
}
