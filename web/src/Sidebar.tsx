import { useMutation } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { button } from "./Modal";
import { jumpKey, SearchIcon } from "./JumpPalette";
import { Named, Now } from "./Now";
import { startShell } from "./api";
import { couldNot, words } from "./labels";
import { loadUnfolded, saveUnfolded } from "./fold";
import { byUrgency, machineNumbers, machineStats, rows, type MachineView, type Row, type WorkspaceView } from "./fleet";
import { MachineName } from "./Machines";
import { glyphText, StateGlyph } from "./StateGlyph";
import { ended } from "./ends";
import { ARCHIVE_ICON, TerminalMenu, useTerminalActions } from "./TerminalMenu";
import { SparkIcon } from "./FirstPrompt";
import { Mark } from "./Mark";
import { ThemeButton } from "./ThemeButton";
import { railKey } from "./collapsed";
import type { MachineSample } from "./api";

const statusColour = { online: "text-neutral-400", offline: "text-red-400", unknown: "text-neutral-400" };
const statusDot = { online: "bg-emerald-400", offline: "bg-red-400", unknown: "bg-neutral-500" };

/** How many states a folded workspace shows on its line before it counts the rest. */
export const STRIP = 6;

interface Props {
  machines: MachineView[];
  /** Opens the New task dialog. */
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
  /** Opens the list of keys. */
  onKeys?: () => void;
  /** Collapses the list to the rail, on a wide screen. */
  onRail?: () => void;
}

/** The terminals a folded workspace (or the rail) shows: the most urgent first, and the open one always. */
export function stripOf(list: Row[], selected?: string): Row[] {
  const ordered = byUrgency(list);
  const shown = ordered.slice(0, STRIP);
  const open = ordered.slice(STRIP).find((r) => r.terminal === selected);
  return open ? [...shown.slice(0, STRIP - 1), open] : shown;
}

/** What a typed name has to start with: the letters typed within this long of each other. */
const TYPEAHEAD_MS = 600;

/**
 * The machines, each with its workspaces and their terminals. The list is one Tab stop: the
 * arrows move between workspaces and terminals (Left folds or goes up to the workspace, Right
 * unfolds or goes in), Home and End go to the ends, and typing a name's first letters goes to the
 * next one it starts. A workspace's actions show while the keyboard is in it, the Tab after it.
 */
export function Sidebar({ machines, onCreate, selected, onNavigate, onClose, onJump, onChanges, alerts, onKeys, onRail }: Props) {
  // A workspace is one line of its states unless it is unfolded: the board already shows every
  // terminal, so the list stays an index. The workspace you are in unfolds, and folds again when
  // you leave it; one you unfold yourself stays open.
  const [unfolded, setUnfolded] = useState(() => new Set(loadUnfolded()));
  const present = new Set(machines.flatMap((m) => m.workspaces.map((w) => w.id)));
  // Keep only workspaces still listed, so archived ones do not pile up in storage.
  const keep = (next: Set<string>) => saveUnfolded([...next].filter((id) => present.has(id)));
  const fold = (ids: string[], on: boolean) =>
    setUnfolded((old) => {
      const next = new Set(old);
      for (const id of ids) {
        if (on) next.delete(id);
        else next.add(id);
      }
      keep(next);
      return next;
    });
  const inside = selected === undefined ? undefined : machines.flatMap((m) => m.workspaces).find((w) => rows(w).some((r) => r.terminal === selected))?.id;
  const [was, setWas] = useState<string>();
  if (inside !== was) {
    setWas(inside);
    const next = new Set(unfolded);
    if (was !== undefined) next.delete(was);
    if (inside !== undefined) next.add(inside);
    setUnfolded(next);
    keep(next);
  }

  // The one Tab stop: the one the keyboard was last in, while it is listed, else the open
  // terminal, else the first workspace.
  const [focused, setFocused] = useState<string>();
  const listed = machines.flatMap((m) =>
    m.workspaces.flatMap((w) => [w.id, ...(!unfolded.has(w.id) ? stripOf(rows(w), selected) : rows(w)).map((r) => r.terminal)]),
  );
  const stop = [focused, selected].find((id) => id !== undefined && listed.includes(id)) ?? listed[0];
  const typed = useRef({ text: "", at: 0 });

  const keys = (event: KeyboardEvent<HTMLElement>) => {
    const here = event.target instanceof HTMLElement ? event.target.closest<HTMLElement>("[data-stop]") : null;
    if (!here || event.metaKey || event.ctrlKey || event.altKey) return;
    const all = [...event.currentTarget.querySelectorAll<HTMLElement>("[data-stop]")];
    const at = all.indexOf(here);
    const workspace = here.dataset.workspace;
    const isWorkspace = here.dataset.stop === workspace;
    let to: HTMLElement | undefined;
    if (event.key === "ArrowDown") to = all[at + 1];
    else if (event.key === "ArrowUp") to = all[at - 1];
    else if (event.key === "Home") to = all[0];
    else if (event.key === "End") to = all.at(-1);
    else if (event.key === "ArrowRight") {
      if (isWorkspace && workspace && !unfolded.has(workspace)) fold([workspace], false);
      else if (isWorkspace || here.dataset.strip !== undefined) to = all[at + 1];
    } else if (event.key === "ArrowLeft") {
      if (isWorkspace && workspace && unfolded.has(workspace)) fold([workspace], true);
      else if (!isWorkspace) to = all.find((e) => e.dataset.stop === workspace);
    } else if (event.key.length === 1 && event.key !== " ") {
      const now = Date.now();
      const text = (now - typed.current.at < TYPEAHEAD_MS ? typed.current.text : "") + event.key.toLowerCase();
      typed.current = { text, at: now };
      // A new first letter looks past this one, so typing it again goes on to the next.
      const from = text.length === 1 ? at + 1 : at;
      const order = [...all.slice(from), ...all.slice(0, from)];
      to = order.find((e) => (e.dataset.name ?? "").toLowerCase().startsWith(text));
      if (!to) return;
    } else return;
    event.preventDefault();
    to?.focus();
  };
  const tab = (id: string) => ({ tabIndex: id === stop ? 0 : -1, onFocus: () => setFocused(id) });

  return (
    <nav id="sidebar" aria-label="Workspaces" onKeyDown={keys} className="flex h-full w-full shrink-0 flex-col gap-4 overflow-y-auto bg-neutral-900 p-3 text-sm text-neutral-100 md:w-80">
      <div className="flex items-center justify-between gap-2">
        <h1 className="text-lg font-semibold">
          <Link to="/" onClick={onNavigate} title="The board" className="flex items-center gap-1.5 rounded">
            <Mark className="size-3.5 text-emerald-400" />
            Foundry
          </Link>
        </h1>
        <div className="flex items-center gap-1.5">
          {onRail && (
            <button
              type="button"
              aria-label="Collapse sidebar"
              title={`Collapse to a rail of states, for a wider terminal (${railKey})`}
              onClick={onRail}
              className="hidden rounded p-1 text-neutral-400 hover:bg-neutral-800 hover:text-neutral-200 md:block"
            >
              <PanelIcon className="size-4" />
            </button>
          )}
          <ThemeButton />
          {alerts}
          {onJump && (
            <button type="button" aria-label="Jump to" title={`Jump to (${jumpKey})`} onClick={onJump} className="rounded p-1 text-neutral-400 hover:bg-neutral-800 hover:text-neutral-200">
              <SearchIcon className="size-4" />
            </button>
          )}
          <button type="button" onClick={onCreate} className={`${button} flex items-center gap-1 whitespace-nowrap`}>
            <SparkIcon className="size-3 text-emerald-400" />
            New task
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
        const allFolded = ids.length > 0 && ids.every((id) => !unfolded.has(id));
        return (
          <section key={machine.id} aria-label={machine.name} className="space-y-2">
            <div className="px-1">
              <div className="group/machine flex items-center gap-2">
                <span aria-hidden="true" className={`size-1.5 shrink-0 rounded-full ${statusDot[machine.status]}`} />
                <MachineName machine={machine} className="text-sm font-semibold">
                  {(rename) => (
                    <>
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
                      {rename && (
                        <Action
                          label={`Rename ${machine.name}`}
                          title="Rename"
                          onClick={rename}
                          className="shrink-0 pointer-fine:opacity-0 pointer-fine:group-hover/machine:opacity-100 pointer-fine:focus-visible:opacity-100"
                        >
                          <path d="M10.5 2.5l3 3L6 13H3v-3z" />
                        </Action>
                      )}
                    </>
                  )}
                </MachineName>
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
                  folded={!unfolded.has(workspace.id)}
                  onFold={(on) => fold([workspace.id], on)}
                  selected={selected}
                  onNavigate={onNavigate}
                  onChanges={onChanges}
                  tab={tab}
                />
              ))}
            </div>
          </section>
        );
      })}
      {onKeys && (
        <button
          type="button"
          onClick={onKeys}
          title="Every key Foundry answers"
          className="mt-auto flex items-center gap-1.5 self-start rounded px-1 text-xs text-neutral-500 hover:text-neutral-200 pointer-coarse:hidden"
        >
          Keyboard
          <kbd className="rounded border border-neutral-700 px-1 font-sans text-[11px] leading-4">?</kbd>
        </button>
      )}
    </nav>
  );
}

/** Load per core as a short bar: grey with room, amber near full, red past one per core. */
function LoadMeter({ sample }: { sample: MachineSample }) {
  const load = sample.load_per_core;
  const tone = load >= 1 ? "bg-red-400" : load >= 0.7 ? "bg-amber-400" : "bg-neutral-400";
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
  /** A stop's place in the list's one Tab stop, by its id (see `Sidebar`). */
  tab: (id: string) => { tabIndex: number; onFocus: () => void };
}

function Workspace({ workspace, machine, folded, onFold, selected, onNavigate, onChanges, tab }: WorkspaceProps) {
  const shell = useMutation({ mutationFn: () => startShell(workspace.id) });
  const terminals = useTerminalActions();
  const list = rows(workspace);
  const over = ended(workspace);
  return (
    <div role="group" aria-label={workspace.name} className="group/ws">
      <div className="flex items-center gap-2 rounded-md px-1 py-1 hover:bg-neutral-800/60">
        <button
          type="button"
          aria-expanded={!folded}
          onClick={() => onFold(!folded)}
          data-stop={workspace.id}
          data-workspace={workspace.id}
          data-name={workspace.name}
          {...tab(workspace.id)}
          title={folded ? "Unfold" : "Fold"}
          className="flex min-w-0 flex-1 items-center gap-1.5 text-left"
        >
          <svg viewBox="0 0 16 16" aria-hidden="true" className={`size-3 shrink-0 text-neutral-500 transition-transform motion-reduce:transition-none ${folded ? "" : "rotate-90"}`} fill="currentColor">
            <path d="M6 3.5l5 4.5-5 4.5z" />
          </svg>
          <span className="truncate font-medium">{workspace.name}</span>
        </button>
        {folded && <Strip list={list} workspace={workspace.id} selected={selected} onNavigate={onNavigate} tab={tab} />}
      </div>
      {!folded && (
        <>
          <div className="flex items-center gap-2 pl-6 pr-1">
            <span className="min-w-0 flex-1 truncate text-xs text-neutral-500">
              {workspace.branch} · {workspace.project}
            </span>
            <div className="flex shrink-0 items-center gap-0.5 pointer-fine:hidden pointer-fine:group-focus-within/ws:flex pointer-fine:group-hover/ws:flex">
              {onChanges && (
                <Action label="Changes" title={words.changes} onClick={() => onChanges(workspace.id)}>
                  <path d="M8 2.5v6M5 5.5h6M5 12.5h6" />
                </Action>
              )}
              {terminals && (
                <Action label="New session" title="Start Claude Code in this workspace" onClick={() => terminals.onNewSession(workspace.id, workspace.name)}>
                  <path d="M8 2.5l1.3 3.2 3.2 1.3-3.2 1.3L8 11.5 6.7 8.3 3.5 7l3.2-1.3z" />
                </Action>
              )}
              <Action label="New shell" title="Open a shell in this workspace" onClick={() => shell.mutate()} disabled={shell.isPending}>
                <path d="M3.5 4.5l3 3-3 3M8 11.5h4.5" />
              </Action>
              {terminals && over.length > 0 && (
                <Action label={`Clear ${over.length} ended`} title="Clear the terminals that ended, with their final screens" onClick={() => terminals.clear(over)}>
                  <path d="M11.5 2l-3 6M4 8h7l1 5.5H3zM6 10.5v3M9 10.5v3" />
                </Action>
              )}
              {terminals?.here === machine && (
                <Action label="Archive" title={words.archive} onClick={() => terminals.archive(workspace.id)}>
                  <path d={ARCHIVE_ICON} />
                </Action>
              )}
            </div>
          </div>
          {shell.error && (
            <p role="alert" className="pl-6 text-xs text-red-400">
              {couldNot("start a shell", shell.error)}
            </p>
          )}
          {list.length > 0 && (
            <ul className="ml-[11px] mt-0.5 border-l border-neutral-800 pl-1.5">
              {list.map((row) => (
                <Item key={row.terminal} row={row} workspace={workspace.id} selected={selected} onNavigate={onNavigate} tab={tab} />
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
function Strip({ list, workspace, selected, onNavigate, tab }: { list: Row[]; workspace: string; selected?: string; onNavigate?: () => void; tab: WorkspaceProps["tab"] }) {
  const shown = stripOf(list, selected);
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
            data-stop={row.terminal}
            data-workspace={workspace}
            data-strip=""
            data-name={row.name}
            {...tab(row.terminal)}
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

function Action({ label, title, onClick, disabled, className = "", children }: { label: string; title: string; onClick?: () => void; disabled?: boolean; className?: string; children: ReactNode }) {
  return (
    <button
      type="button"
      aria-label={label}
      title={title}
      onClick={onClick}
      disabled={disabled}
      className={`grid size-6 place-items-center rounded text-neutral-400 hover:bg-neutral-700 hover:text-neutral-100 disabled:opacity-40 ${className}`}
    >
      <svg viewBox="0 0 16 16" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
        {children}
      </svg>
    </button>
  );
}

function Item({ row, workspace, selected, onNavigate, tab }: { row: Row; workspace: string; selected?: string; onNavigate?: () => void; tab: WorkspaceProps["tab"] }) {
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
          data-stop={row.terminal}
          data-workspace={workspace}
          data-name={row.title ? `${row.name} ${row.title}` : row.name}
          {...tab(row.terminal)}
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

/** A window with its left column marked: the sidebar, collapsed or expanded. */
export function PanelIcon({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 16 16" className={className} fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
      <rect x="2" y="2.5" width="12" height="11" rx="1.5" />
      <path d="M6 2.5v11" />
    </svg>
  );
}
