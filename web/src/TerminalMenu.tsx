import { useMutation, useQuery } from "@tanstack/react-query";
import { ContextMenu, Dialog, DropdownMenu } from "radix-ui";
import { createContext, Fragment, useContext, useEffect, useRef, useState, type ComponentType, type PointerEvent, type ReactElement, type ReactNode } from "react";
import { closeTerminal, machineQuery, startShell } from "./api";
import { couldNot } from "./labels";
import { ArchiveDialog } from "./Archive";
import { ended, endOf } from "./ends";
import { matches, MD } from "./media";
import { locate, type Located, type MachineView } from "./fleet";
import { button, dangerButton, Modal } from "./Modal";
import { StateGlyph } from "./StateGlyph";

interface Actions {
  machines: MachineView[];
  /** The terminal with the keyboard. */
  current?: string;
  onOpen: (id: string) => void;
  /** Shows terminal `id` beside the one with the keyboard. */
  onBeside: (id: string) => void;
  /** Opens terminal `id` with its steps column. */
  onSteps: (id: string) => void;
  onChanges: (workspace: string) => void;
  /** Opens the find bar over the terminal with the keyboard. */
  onFind: () => void;
  /** Asks for a new session in `workspace`, named `name`. */
  onNewSession: (workspace: string, name: string) => void;
}

/** Where a session's name is edited in place: its sidebar row or its tab. */
export type Place = "row" | "tab";

export interface Context extends Actions {
  /** The machine serving the UI, once known: only its workspaces get new terminals or are archived from here. */
  here?: string;
  /** The session name being edited, by its terminal, and where. */
  renaming?: { terminal: string; place: Place };
  /**
   * Edits the name of the session in terminal `id` at `place`; with no place, in its tab, opening
   * the terminal first when it is not the one with the keyboard.
   */
  rename: (id: string, place?: Place) => void;
  /** Closes the name being edited. */
  stopRenaming: () => void;
  /** Ends terminal `id`: at once when it already ended, else after asking. */
  end: (id: string) => void;
  /** Removes ended terminals `ids`. */
  clear: (ids: string[]) => void;
  /** Starts a shell in `workspace` and opens it. */
  shell: (workspace: string) => void;
  /** Asks to archive `workspace`, showing what stays, what is lost and what stops. */
  archive: (workspace: string) => void;
}

/** How long the line saying a workspace was archived stays. */
export const ARCHIVED_FOR = 6000;

const Terminals = createContext<Context | undefined>(undefined);

/**
 * Gives every terminal row inside it (sidebar, tabs, board) a menu of what can be done with that
 * terminal: right-click it, or press and hold on a touch screen. It holds the ending of
 * terminals too: one that still runs is ended after asking, one that already ended is cleared at
 * once, and a request the daemon refuses says why at the foot of the screen. The daemon carries
 * an ending to the machine that owns the terminal, so any reachable machine's terminals are ended
 * from here. Only the machine serving the UI is offered new terminals in its workspaces.
 */
export function TerminalActions({ children, ...actions }: Actions & { children: ReactNode }) {
  const [asking, setAsking] = useState<string>();
  const [failed, setFailed] = useState<string>();
  const [archiving, setArchiving] = useState<string>();
  const [archived, setArchived] = useState<{ name: string; branch: string }>();
  const [renaming, setRenaming] = useState<{ terminal: string; place: Place }>();
  useEffect(() => {
    if (!archived) return;
    const timer = setTimeout(() => setArchived(undefined), ARCHIVED_FOR);
    return () => clearTimeout(timer);
  }, [archived]);
  const clearing = useMutation({
    mutationFn: (ids: string[]) => Promise.all(ids.map(closeTerminal)),
    onError: (error, ids) => setFailed(couldNot(ids.length === 1 ? "clear the terminal" : `clear ${ids.length} ended terminals`, error)),
  });
  const shell = useMutation({
    mutationFn: startShell,
    onSuccess: actions.onOpen,
    onError: (error) => setFailed(couldNot("start a shell", error)),
  });
  const { data: here } = useQuery(machineQuery);
  const context: Context = {
    ...actions,
    here,
    end: (id) => {
      const found = locate(actions.machines, id);
      if (found && endOf(found.glyph) === "clear") clearing.mutate([id]);
      else if (found) setAsking(id);
    },
    clear: (ids) => clearing.mutate(ids),
    shell: (workspace) => shell.mutate(workspace),
    archive: setArchiving,
    renaming,
    rename: (id, place) => {
      if (!place && id !== actions.current) actions.onOpen(id);
      setRenaming({ terminal: id, place: place ?? "tab" });
    },
    stopRenaming: () => setRenaming(undefined),
  };
  const target = asking ? locate(actions.machines, asking) : undefined;
  const leaving = archiving ? actions.machines.flatMap((m) => m.workspaces).find((w) => w.id === archiving) : undefined;
  return (
    <Terminals.Provider value={context}>
      {children}
      {asking && target && <EndDialog key={asking} id={asking} found={target} onDone={() => setAsking(undefined)} />}
      {leaving && <ArchiveDialog key={leaving.id} workspace={leaving} onClose={() => setArchiving(undefined)} onArchived={(name, branch) => setArchived({ name, branch })} />}
      {archived && (
        <p
          role="status"
          className="fixed inset-x-3 bottom-3 z-50 mx-auto flex max-w-md items-center gap-2.5 rounded-lg border border-emerald-400/40 bg-neutral-900 px-3 py-2 text-xs text-neutral-200 shadow-xl"
        >
          <svg viewBox="0 0 16 16" className="size-4 shrink-0 text-emerald-400" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
            <path d="M3.5 8.5l3 3 6-7" />
          </svg>
          <span className="min-w-0 flex-1">
            Archived <span className="font-semibold">{archived.name}</span>. Branch <code className="font-mono text-emerald-300">{archived.branch}</code> kept.
          </span>
          <button type="button" onClick={() => setArchived(undefined)} className="shrink-0 rounded px-1.5 py-0.5 text-neutral-400 hover:bg-neutral-800 hover:text-neutral-100">
            Dismiss
          </button>
        </p>
      )}
      {failed && (
        <p
          role="alert"
          className="fixed inset-x-3 bottom-3 z-50 mx-auto flex max-w-md items-center gap-3 rounded-lg border border-red-400/40 bg-neutral-900 px-3 py-2 text-xs text-red-300 shadow-xl"
        >
          <span className="min-w-0 flex-1">{failed}</span>
          <button type="button" onClick={() => setFailed(undefined)} className="shrink-0 rounded px-1.5 py-0.5 text-neutral-400 hover:bg-neutral-800 hover:text-neutral-100">
            Dismiss
          </button>
        </p>
      )}
    </Terminals.Provider>
  );
}

/** Whether the screen is wide: room for a second terminal beside the first, and for Find in the header. */
function wide(): boolean {
  return matches(MD, true);
}

const menu =
  "z-50 min-w-56 max-w-[min(20rem,calc(100vw-1rem))] overflow-hidden rounded-lg border border-neutral-700 bg-neutral-900 p-1 text-sm text-neutral-200 shadow-xl shadow-black/40";
const item = "flex cursor-default select-none items-center gap-2.5 rounded-md px-2 py-1.5 outline-none data-highlighted:bg-neutral-800 data-highlighted:text-neutral-50";

interface Kit {
  Item: ComponentType<{ onSelect?: () => void; onPointerUp?: (event: PointerEvent) => void; className?: string; children: ReactNode }>;
  Separator: ComponentType<{ className?: string }>;
  Label: ComponentType<{ className?: string; children: ReactNode }>;
}
const contextKit: Kit = { Item: ContextMenu.Item, Separator: ContextMenu.Separator, Label: ContextMenu.Label };
const dropdownKit: Kit = { Item: DropdownMenu.Item, Separator: DropdownMenu.Separator, Label: DropdownMenu.Label };

/**
 * The content's focus handlers: closed by Escape, the keyboard goes back to what opened the menu;
 * closed by a pick, it stays where the pick put it (the find box, a dialog, the terminal).
 */
function useReturnFocus() {
  const escaped = useRef(false);
  return {
    onEscapeKeyDown: () => {
      escaped.current = true;
    },
    onCloseAutoFocus: (event: Event) => {
      if (!escaped.current) event.preventDefault();
      escaped.current = false;
    },
  };
}

/**
 * Props for an item that ends or clears: picked only by a click (a press and a release on it) or
 * the keyboard. Radix also picks an item on a release it never saw pressed, so the release of the
 * right-click or long press that opened the menu under the pointer would pick whatever lies there.
 */
const deliberate = { onPointerUp: (event: PointerEvent) => event.preventDefault() };

/** A box with its lid: archiving a workspace. */
export const ARCHIVE_ICON = "M2.5 3.5h11v3h-11zM3.5 6.5v6h9v-6M6.5 9h3";

/** One thing that can be done with a terminal, as its menu and the jump palette list it. */
export interface Act {
  key: string;
  label: string;
  /** The icon's SVG path, on a 16 by 16 box. */
  icon: string;
  run: () => void;
  /** Acts after a group of them start a new group in a menu. */
  group: "go" | "new" | "end";
  /** Ends a child that still runs. */
  danger?: boolean;
}

/**
 * What can be done with terminal `id` from here, in menu order: going to it or to what belongs to
 * it, and renaming a session, starting something new beside it, and ending it. Any reachable
 * machine's terminals can be ended and its sessions renamed; only this machine's workspaces get
 * new terminals or are archived.
 */
export function actsFor(id: string, at: Context, found: Located, roomy = wide()): Act[] {
  const mine = found.machine.id === at.here;
  const kind = endOf(found.glyph);
  const rest = ended(found.workspace);
  const { id: workspace, name } = found.workspace;
  const acts: (Act | false | undefined)[] = [
    id !== at.current && { key: "open", label: "Open", icon: "M3 3.5h10v9H3zM3 6h10", run: () => at.onOpen(id), group: "go" },
    !!at.current && id !== at.current && roomy && { key: "beside", label: "Open beside", icon: "M2.5 3h11v10h-11zM8 3v10", run: () => at.onBeside(id), group: "go" },
    !!found.session && { key: "steps", label: "Steps", icon: "M3 4h2M3 8h2M3 12h2M7 4h6M7 8h6M7 12h4", run: () => at.onSteps(id), group: "go" },
    !!found.session && { key: "rename", label: "Rename", icon: "M10.5 2.5l3 3L6 13H3v-3z", run: () => at.rename(id), group: "go" },
    id === at.current &&
      !roomy && {
        key: "find",
        label: "Find in scrollback",
        icon: "M2 3.5h10M2 7h5M2 10.5h3.5M12.5 12l1.75 1.75M13.25 10a2.75 2.75 0 1 1-5.5 0 2.75 2.75 0 0 1 5.5 0",
        run: at.onFind,
        group: "go",
      },
    { key: "changes", label: `Changes in ${name}`, icon: "M8 2.5v6M5 5.5h6M5 12.5h6", run: () => at.onChanges(workspace), group: "go" },
    mine && { key: "session", label: `New session in ${name}`, icon: "M8 2.5l1.4 4.1 4.1 1.4-4.1 1.4L8 13.5l-1.4-4.1L2.5 8l4.1-1.4z", run: () => at.onNewSession(workspace, name), group: "new" },
    mine && { key: "shell", label: `New shell in ${name}`, icon: "M2.5 3h11v10h-11zM5 6.5l1.75 1.5L5 9.5M8.5 9.5h2.5", run: () => at.shell(workspace), group: "new" },
    kind === "end" && { key: "end", label: `End ${found.name}…`, icon: "M8 2.5v5M4.8 4.6a4.8 4.8 0 1 0 6.4 0", run: () => at.end(id), group: "end", danger: true },
    kind === "clear" && { key: "clear", label: "Clear", icon: "M3.5 4.5h9M6.5 4.5V3h3v1.5M4.5 4.5l.7 8.5h5.6l.7-8.5", run: () => at.end(id), group: "end" },
    rest.length > (kind === "clear" ? 1 : 0) && {
      key: "sweep",
      label: `Clear ${rest.length} ended in ${name}`,
      icon: "M11.5 2l-3 6M4 8h7l1 5.5H3zM6 10.5v3M9 10.5v3",
      run: () => at.clear(rest),
      group: "end",
    },
    mine && { key: "archive", label: `Archive ${name}…`, icon: ARCHIVE_ICON, run: () => at.archive(workspace), group: "end" },
  ];
  return acts.filter((act): act is Act => !!act);
}

/** The menu's entries for terminal `id`, as a right-click menu or a button's menu draws them. */
function Entries({ kit: { Item, Separator, Label }, id, at, found }: { kit: Kit; id: string; at: Context; found: Located }) {
  const acts = actsFor(id, at, found);
  return (
    <>
      <Label className="flex min-w-0 items-center gap-2 px-2 pt-1 pb-1.5 text-xs text-neutral-500">
        <StateGlyph glyph={found.glyph} still />
        <span className="truncate">
          {found.workspace.name} / {found.title ?? found.name}
        </span>
      </Label>
      {acts.map((act, i) => (
        <Fragment key={act.key}>
          {i > 0 && act.group !== acts[i - 1].group && <Separator className="my-1 h-px bg-neutral-800" />}
          <Item
            {...(act.group === "end" ? deliberate : {})}
            onSelect={act.run}
            className={act.danger ? `${item} text-red-400 data-highlighted:bg-red-400/10 data-highlighted:text-red-300` : item}
          >
            <Icon d={act.icon} />
            {act.label}
          </Item>
        </Fragment>
      ))}
    </>
  );
}

export function Icon({ d }: { d: string }) {
  return (
    <svg viewBox="0 0 16 16" className="size-3.5 shrink-0 text-neutral-500" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d={d} />
    </svg>
  );
}

/** `at` with its rename editing at `place`, the row the menu was opened from, when there is one. */
function renamingAt(at: Context, place: Place | undefined): Context {
  return place ? { ...at, rename: (id) => at.rename(id, place) } : at;
}

/**
 * Terminal `id`'s row, which opens its menu on a right-click or a long press. Outside
 * `TerminalActions`, or for a terminal no longer listed, the row is shown as it is. A row that
 * shows a session's name says which `place` it is, so the menu's Rename edits it there.
 */
export function TerminalMenu({ id, place, children }: { id: string; place?: Place; children: ReactElement }) {
  const terminals = useContext(Terminals);
  const at = terminals && renamingAt(terminals, place);
  const focus = useReturnFocus();
  const found = at && locate(at.machines, id);
  if (!at || !found) return children;
  return (
    <ContextMenu.Root modal={false}>
      <ContextMenu.Trigger asChild className="[-webkit-touch-callout:none] data-[state=open]:bg-neutral-800">
        {children}
      </ContextMenu.Trigger>
      <ContextMenu.Portal>
        <ContextMenu.Content className={menu} collisionPadding={8} {...focus}>
          <Entries kit={contextKit} id={id} at={at} found={found} />
        </ContextMenu.Content>
      </ContextMenu.Portal>
    </ContextMenu.Root>
  );
}

/**
 * The same menu from a button, for the terminal's header: the way in on a touch screen. Its Rename
 * edits the name in the open tab.
 */
export function TerminalMenuButton({ id }: { id: string }) {
  const terminals = useContext(Terminals);
  const at = terminals && renamingAt(terminals, "tab");
  const focus = useReturnFocus();
  const found = at && locate(at.machines, id);
  if (!at || !found) return null;
  return (
    <DropdownMenu.Root modal={false}>
      <DropdownMenu.Trigger
        aria-label="More"
        title="More: open beside, steps, rename, changes, end"
        className="shrink-0 rounded p-1.5 text-neutral-400 hover:bg-neutral-800 hover:text-neutral-200 data-[state=open]:bg-neutral-800 data-[state=open]:text-neutral-100"
      >
        <svg viewBox="0 0 16 16" className="size-4" fill="currentColor" aria-hidden="true">
          <circle cx="3.5" cy="8" r="1.25" />
          <circle cx="8" cy="8" r="1.25" />
          <circle cx="12.5" cy="8" r="1.25" />
        </svg>
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content align="end" sideOffset={4} collisionPadding={8} className={menu} {...focus}>
          <Entries kit={dropdownKit} id={id} at={at} found={found} />
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}

/** What can be done with terminals, inside `TerminalActions`. */
export function useTerminalActions(): Context | undefined {
  return useContext(Terminals);
}

/** Asks before ending a terminal whose child still runs, saying what is lost. */
function EndDialog({ id, found, onDone }: { id: string; found: Located; onDone: () => void }) {
  const end = useMutation({ mutationFn: () => closeTerminal(id), onSuccess: onDone });
  const what = !found.session
    ? "Whatever runs in it stops and the terminal closes."
    : found.glyph === "working"
      ? "It is in the middle of a turn: the turn stops where it is and Claude Code closes."
      : found.glyph === "waiting"
        ? "It is waiting for your answer: Claude Code closes without one."
        : "Claude Code closes, and so does its terminal.";
  return (
    <Modal open onOpenChange={(open) => !open && onDone()} title={`End ${found.name}?`} description={`${what} The workspace, its worktree and its branch stay.`}>
      <p className="mt-3 flex min-w-0 items-center gap-2 rounded border border-neutral-800 bg-neutral-950 px-2 py-1.5 text-xs text-neutral-300">
        <StateGlyph glyph={found.glyph} />
        <span className="truncate">
          {found.workspace.name} / {found.title ?? found.name}
        </span>
        <span className="ml-auto shrink-0 text-neutral-500">{found.label}</span>
      </p>
      {end.isError && (
        <p role="alert" className="mt-2 text-xs text-red-400">
          {couldNot(`end ${found.name}`, end.error)}
        </p>
      )}
      <div className="mt-4 flex justify-end gap-2">
        <Dialog.Close className={button}>Cancel</Dialog.Close>
        <button
          type="button"
          onClick={() => end.mutate()}
          disabled={end.isPending}
          className={dangerButton}
        >
          End
        </button>
      </div>
    </Modal>
  );
}
