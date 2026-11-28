import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { button } from "./Modal";
import { jumpKey, SearchIcon } from "./JumpPalette";
import { Named } from "./Now";
import { startShell } from "./api";
import { archiveOf } from "./Archive";
import { couldNot, words } from "./labels";
import { loadUnfolded, saveUnfolded } from "./fold";
import { branchOf, byUrgency, machineNumbers, machineStats, rows, type MachineView, type Row, type WorkspaceView } from "./fleet";
import { FactoryIcon, FactoryMark } from "./FactoryMark";
import { MachineName } from "./Machines";
import { glyphFill, glyphText, StateGlyph } from "./StateGlyph";
import { ended } from "./ends";
import { useLeaving } from "./Archive";
import { ARCHIVE_ICON, TerminalMenu, useTerminalActions } from "./TerminalMenu";
import { SessionNameField, useRenaming } from "./SessionName";
import { SparkIcon } from "./FirstPrompt";
import { Mark } from "./Mark";
import { ProjectFace } from "./ProjectFace";
import { SettingsLink, ThemeButton } from "./ThemeButton";
import { railKey } from "./collapsed";
import { pollEvery } from "./poll";
import { CHECKS_EVERY, checksPending, SHIPPING_STALE, shippingQuery, standing, toneText, useReadOnChange } from "./Shipping";
import { useOnScreen } from "./cardChanges";
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
  /** Whether the fleet shown is not the daemon's live word (remembered, or the link is lost): a machine's sweep waits for it. */
  stale?: boolean;
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
export function Sidebar({ machines, onCreate, selected, onNavigate, onClose, onJump, onChanges, alerts, onKeys, onRail, stale }: Props) {
  // A workspace is one line of its states unless it is unfolded: the board already shows every
  // terminal, so the list stays an index. The workspace you are in unfolds, and folds again when
  // you leave it; one you unfold yourself stays open.
  const [unfolded, setUnfolded] = useState(() => new Set(loadUnfolded()));
  const terminals = useTerminalActions();
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

  // The header sits above the list that scrolls, so a scrollbar takes its width from the list
  // and the header always has the sidebar's full width for its buttons.
  return (
    <nav id="sidebar" aria-label="Workspaces" onKeyDown={keys} className="flex h-full w-full shrink-0 flex-col bg-neutral-900 text-sm text-neutral-100 md:w-80">
      <div className="flex shrink-0 items-center justify-between gap-2 px-3 pt-3">
        <h1 className="min-w-0 text-lg font-semibold">
          <Link to="/" onClick={onNavigate} title="The board" className="flex min-w-0 items-center gap-1.5 rounded">
            <Mark className="size-3.5 shrink-0 text-emerald-400" />
            <span className="truncate">Foundry</span>
          </Link>
        </h1>
        <div className="flex shrink-0 items-center gap-0.5">
          <Link
            to="/factory"
            onClick={onNavigate}
            aria-label="The factory"
            title="The factory"
            className="rounded p-1 text-neutral-400 hover:bg-neutral-800 hover:text-neutral-200"
          >
            <FactoryIcon className="size-4" />
          </Link>
          {onRail && (
            <button
              type="button"
              aria-label="Collapse sidebar"
              title={`Collapse to a rail, for a wider terminal (${railKey})`}
              onClick={onRail}
              className="hidden rounded p-1 text-neutral-400 hover:bg-neutral-800 hover:text-neutral-200 md:block"
            >
              <PanelIcon className="size-4" />
            </button>
          )}
          <ThemeButton />
          <SettingsLink onClick={onNavigate} />
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
      <div className="flex min-h-0 flex-1 flex-col gap-4 overflow-y-auto p-3 pt-4">
        {machines.map((machine) => {
          const ids = machine.workspaces.map((w) => w.id);
          const allFolded = ids.length > 0 && ids.every((id) => !unfolded.has(id));
          // Each workspace's own Clear N ended for the whole machine, so failed shells count too, though the Sessions list files them under Needs you.
          const over = machine.workspaces.flatMap(ended);
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
                {terminals && <Tidy over={over} empty={sweepable(machine)} clear={terminals.clear} stale={stale} />}
              </div>
              <div className="space-y-3">
                {machine.workspaces.map((workspace) => (
                  <Workspace
                    key={workspace.id}
                    workspace={workspace}
                    folded={!unfolded.has(workspace.id)}
                    readable={machine.status !== "offline"}
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
      </div>
    </nav>
  );
}

/**
 * The workspaces of `machine` its sweep archives: those with no terminals left that no factory run
 * holds. A run's workspace can be empty between its sessions, and the factory still builds in it;
 * while its project's status is not known yet (`statusUnknown`), whether a run holds it is not known either.
 */
function sweepable(machine: MachineView): string[] {
  return machine.workspaces.filter((w) => rows(w).length === 0 && !w.factory && !w.statusUnknown).map((w) => w.id);
}

/**
 * A machine's sweep, under its numbers: clear every terminal that ended (`over`), and archive every
 * workspace left with no terminals (`empty`). Archiving is not forced, so the daemon keeps any
 * workspace with work it would lose, and the line says how many it kept. Each archive is the same
 * mutation a single one is (`archiveOf`), so every view sees it under way; one that fails leaves
 * the others to finish, and the line says how many could not be archived and why. While the fleet
 * shown is not the daemon's live word (`stale`), both wait: what it lists as ended or empty may not be.
 */
function Tidy({ over, empty, clear, stale = false }: { over: string[]; empty: string[]; clear: (ids: string[]) => void; stale?: boolean }) {
  const client = useQueryClient();
  const archive = useMutation({
    mutationFn: async (ids: string[]) => {
      const results = await Promise.allSettled(ids.map((id) => client.getMutationCache().build(client, archiveOf(id)).execute(false)));
      const failed = results.flatMap((r) => (r.status === "rejected" ? [r.reason as unknown] : []));
      return { of: ids.length, kept: results.filter((r) => r.status === "fulfilled" && r.value !== null).length, failed: failed.length, why: failed[0] };
    },
  });
  if (over.length === 0 && empty.length === 0) return null;
  const action = "rounded px-1 text-[11px] text-neutral-500 hover:bg-neutral-800 hover:text-neutral-200 disabled:opacity-50";
  // What was kept or could not be archived is still empty, so the line stays until those go too.
  const { kept = 0, failed = 0, of = 0, why = undefined } = archive.data ?? {};
  const said = [
    kept > 0 && `${kept} kept: ${kept === 1 ? "it has" : "they have"} work archiving would lose. Archive ${kept === 1 ? "it from its row" : "them from their rows"}.`,
    failed > 0 && couldNot(`archive ${failed} of the ${of} empty workspaces`, why),
  ].filter((line) => line !== false);
  return (
    <div className="mt-1 flex flex-wrap items-center gap-x-1">
      {over.length > 0 && (
        <button
          type="button"
          onClick={() => clear(over)}
          disabled={stale}
          title="Clear the terminals that ended in every workspace on this machine, with their final screens"
          className={action}
        >
          Clear {over.length} ended
        </button>
      )}
      {empty.length > 0 && (
        // One click and no confirm: the archive is never forced, so the daemon keeps any workspace with work to lose, and branches stay.
        <button
          type="button"
          onClick={() => archive.mutate(empty)}
          disabled={stale || archive.isPending}
          title="Archive every workspace on this machine with no terminals left. Their branches stay; one with work archiving would lose is kept."
          className={action}
        >
          {archive.isPending ? `Archiving ${archive.variables.length}…` : `Archive ${empty.length} empty`}
        </button>
      )}
      {empty.length > 0 && said.length > 0 && (
        <p role="status" className="basis-full px-1 text-xs text-amber-400">
          {said.join(" ")}
        </p>
      )}
    </div>
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
  folded: boolean;
  /** Whether its machine may be asked where its work stands: any but an offline one. */
  readable: boolean;
  onFold: (on: boolean) => void;
  selected?: string;
  onNavigate?: () => void;
  onChanges?: (workspace: string) => void;
  /** A stop's place in the list's one Tab stop, by its id (see `Sidebar`). */
  tab: (id: string) => { tabIndex: number; onFocus: () => void };
}

/**
 * A workspace in two columns: its project's face, with a spine hanging from it in its most urgent
 * terminal's fill (the board's card edge, so the two always match), and beside it everything else.
 * The face sits in the fold button with the name, drawn over its own column, so they are one stop.
 * Unfolded, it is two lines above its sessions: the name with its actions and factory mark, then
 * the standing line ending in the branch.
 */
function Workspace({ workspace, folded, readable, onFold, selected, onNavigate, onChanges, tab }: WorkspaceProps) {
  const shell = useMutation({ mutationFn: () => startShell(workspace.id) });
  const terminals = useTerminalActions();
  const list = rows(workspace);
  const over = ended(workspace);
  const lead = byUrgency(list)[0];
  const branch = branchOf(workspace);
  return (
    <div role="group" aria-label={workspace.name} className="group/ws relative grid grid-cols-[1.25rem_minmax(0,1fr)] gap-x-2">
      <span aria-hidden="true" data-spine="" className={`mx-auto mt-7 mb-[3px] w-0.5 rounded-full ${lead ? glyphFill[lead.glyph] : "bg-neutral-800"}`} />
      <div className="min-w-0">
        {/* At least a 24px action tall, so the actions showing on hover do not move the list. */}
        <div className="flex min-h-7 items-center gap-2 rounded-md py-0.5 pr-1 pl-2 hover:bg-neutral-800/60">
          <button
            type="button"
            aria-expanded={!folded}
            onClick={() => onFold(!folded)}
            data-stop={workspace.id}
            data-workspace={workspace.id}
            data-name={workspace.name}
            {...tab(workspace.id)}
            title={folded ? "Unfold" : "Fold"}
            className="flex min-w-0 flex-1 items-center text-left"
          >
            <ProjectFace project={workspace.project} className="absolute top-1 left-0 size-5 text-[10px]" />
            <span className="truncate font-medium">{workspace.name}</span>
          </button>
          {!folded && (
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
              {terminals && (
                <Action label="Archive" title={words.archive} onClick={() => terminals.archive(workspace.id)}>
                  <path d={ARCHIVE_ICON} />
                </Action>
              )}
            </div>
          )}
          {workspace.factory && <FactoryMark run={workspace.factory} compact />}
          {folded && <Strip list={list} workspace={workspace.id} selected={selected} onNavigate={onNavigate} tab={tab} />}
        </div>
        <StandingLine workspace={workspace} list={list} readable={readable} branch={folded || branch === workspace.name ? undefined : branch} />
        {!folded && (
          <>
            {shell.error && (
              <p role="alert" className="pl-2 text-xs text-red-400">
                {couldNot("start a shell", shell.error)}
              </p>
            )}
            {list.length > 0 && (
              <ul className="mt-0.5">
                {list.map((row) => (
                  <Item key={row.terminal} row={row} workspace={workspace.id} selected={selected} onNavigate={onNavigate} tab={tab} />
                ))}
              </ul>
            )}
          </>
        )}
      </div>
    </div>
  );
}

/**
 * The line under a workspace's name: its project, then where its work stands (`standing`) in its
 * tone, the pull request linked to GitHub. Read through the cache entry the board shares, once the
 * row is on screen, again when one of its terminals changes glyph (a turn that ends may have pushed
 * or opened a pull request), and every `CHECKS_EVERY` only while its pull request's checks run;
 * never on window focus or reconnect, since each read runs git and asks GitHub on the owner. Not
 * read while `readable` is off (its machine is offline). A read that fails or has not answered
 * leaves the project alone, and says nothing of why. `branch`, when given, ends the line. While the
 * workspace is being archived, the line says so in place of where it stands, and reads nothing.
 */
function StandingLine({ workspace, list, readable, branch }: { workspace: WorkspaceView; list: Row[]; readable: boolean; branch?: string }) {
  const [ref, onScreen] = useOnScreen<HTMLParagraphElement>();
  const leaving = useLeaving(workspace.id);
  const enabled = readable && onScreen && !leaving;
  const { data, isError } = useQuery({
    ...shippingQuery(workspace.id),
    enabled,
    staleTime: SHIPPING_STALE,
    retry: false,
    refetchOnWindowFocus: false,
    refetchOnReconnect: false,
    refetchInterval: (query) => pollEvery(CHECKS_EVERY, enabled && checksPending(query.state.data))(query),
  });
  // The row reads on a glyph change as a board card does (#311); the hook reads once when both do.
  useReadOnChange(workspace.id, list.map((r) => `${r.terminal}:${r.glyph}`).join(" "));

  const working = list.some((r) => r.glyph === "starting" || r.glyph === "working");
  const stands = !leaving && readable && data && !isError ? standing(data, working) : undefined;
  const title = [workspace.project, leaving ? "Archiving…" : stands?.text, branch].filter((part) => part !== undefined).join(" · ");
  return (
    <p ref={ref} title={title} className="truncate pr-1 pl-2 text-xs text-neutral-500">
      <span>{workspace.project}</span>
      {leaving && (
        <>
          <span className="text-neutral-600"> · </span>
          <span>Archiving…</span>
        </>
      )}
      {stands && (
        <>
          <span className="text-neutral-600"> · </span>
          {stands.pr ? (
            <a
              href={stands.pr.url}
              target="_blank"
              rel="noreferrer"
              title={`${stands.pr.title} (open on GitHub)`}
              tabIndex={-1}
              className={`rounded hover:underline hover:underline-offset-4 ${toneText[stands.tone]}`}
            >
              {stands.text}
            </a>
          ) : (
            <span className={toneText[stands.tone]}>{stands.text}</span>
          )}
        </>
      )}
      {branch !== undefined && (
        <>
          <span className="text-neutral-600"> · </span>
          <span className="font-mono text-[11px]">{branch}</span>
        </>
      )}
    </p>
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
  const naming = useRenaming(row.terminal, "row");
  if (naming?.editing && row.session) {
    return (
      <li className="flex items-center gap-2 rounded bg-neutral-800 px-2 py-1 md:py-0.5">
        <StateGlyph glyph={row.glyph} />
        <SessionNameField session={row.session} name={row.title ?? ""} onClose={naming.stop} className="text-sm" />
      </li>
    );
  }
  return (
    <li>
      <TerminalMenu id={row.terminal} place="row">
        <Link
          to="/terminals/$terminalId"
          params={{ terminalId: row.terminal }}
          aria-current={current ? "true" : undefined}
          onClick={onNavigate}
          data-stop={row.terminal}
          data-workspace={workspace}
          data-name={row.title ? `${row.name} ${row.title}` : row.name}
          {...tab(row.terminal)}
          onDoubleClick={row.session ? naming?.start : undefined}
          title={row.title ? `${row.name}: ${row.title}` : undefined}
          className={`flex items-center gap-2 rounded px-2 py-1.5 hover:bg-neutral-800 md:py-1 ${current ? "bg-neutral-800 text-neutral-50" : ended ? "text-neutral-500" : ""}`}
        >
          <StateGlyph glyph={row.glyph} />
          <span className="flex min-w-0 flex-1 items-baseline gap-2 leading-5">
            {/* Busy: the step takes the title's place on the line; the title stays for screen readers and the tooltip. */}
            <Named name={row.name} title={row.title} step={row.now} />{" "}
            <span className={`shrink-0 text-xs ${ended ? "" : glyphText[row.glyph]}`}>{row.label}</span>
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
