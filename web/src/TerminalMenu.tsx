import { useMutation } from "@tanstack/react-query";
import { ContextMenu, Dialog, DropdownMenu } from "radix-ui";
import { createContext, useContext, useRef, useState, type ComponentType, type ReactElement, type ReactNode } from "react";
import { closeTerminal } from "./api";
import { ended, endOf } from "./ends";
import { locate, type Located, type MachineView } from "./fleet";
import { button, Modal } from "./Modal";
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
}

interface Context extends Actions {
  /** Ends terminal `id`: at once when it already ended, else after asking. */
  end: (id: string) => void;
  /** Removes ended terminals `ids`. */
  clear: (ids: string[]) => void;
}

const Terminals = createContext<Context | undefined>(undefined);

/**
 * Gives every terminal row inside it (sidebar, tabs, board) a menu of what can be done with that
 * terminal: right-click it, or press and hold on a touch screen. It holds the ending of
 * terminals too: one that still runs is ended after asking, one that already ended is cleared at
 * once, and a request the daemon refuses says why at the foot of the screen.
 */
export function TerminalActions({ children, ...actions }: Actions & { children: ReactNode }) {
  const [asking, setAsking] = useState<string>();
  const clearing = useMutation({ mutationFn: (ids: string[]) => Promise.all(ids.map(closeTerminal)) });
  const context: Context = {
    ...actions,
    end: (id) => {
      const found = locate(actions.machines, id);
      if (found && endOf(found.glyph) === "clear") clearing.mutate([id]);
      else if (found) setAsking(id);
    },
    clear: (ids) => clearing.mutate(ids),
  };
  const target = asking ? locate(actions.machines, asking) : undefined;
  return (
    <Terminals.Provider value={context}>
      {children}
      {asking && target && <EndDialog key={asking} id={asking} found={target} onDone={() => setAsking(undefined)} />}
      {clearing.isError && (
        <p
          role="alert"
          className="fixed inset-x-3 bottom-3 z-50 mx-auto flex max-w-md items-center gap-3 rounded-lg border border-red-400/40 bg-neutral-900 px-3 py-2 text-xs text-red-300 shadow-xl"
        >
          <span className="min-w-0 flex-1">Could not end it: {clearing.error.message}</span>
          <button type="button" onClick={() => clearing.reset()} className="shrink-0 rounded px-1.5 py-0.5 text-neutral-400 hover:bg-neutral-800 hover:text-neutral-100">
            Dismiss
          </button>
        </p>
      )}
    </Terminals.Provider>
  );
}

/** Whether the screen is wide: room for a second terminal beside the first, and for Find in the header. */
function wide(): boolean {
  return typeof matchMedia !== "function" || matchMedia("(min-width: 768px)").matches;
}

const menu =
  "z-50 min-w-56 max-w-[min(20rem,calc(100vw-1rem))] overflow-hidden rounded-lg border border-neutral-700 bg-neutral-900 p-1 text-sm text-neutral-200 shadow-xl shadow-black/40";
const item = "flex cursor-default select-none items-center gap-2.5 rounded-md px-2 py-1.5 outline-none data-highlighted:bg-neutral-800 data-highlighted:text-neutral-50";

interface Kit {
  Item: ComponentType<{ onSelect?: () => void; className?: string; children: ReactNode }>;
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

/** The menu's entries for terminal `id`, as a right-click menu or a button's menu draws them. */
function Entries({ kit: { Item, Separator, Label }, id, at, found }: { kit: Kit; id: string; at: Context; found: Located }) {
  const kind = endOf(found.glyph);
  const rest = ended(found.workspace);
  const sweep = rest.length > (kind === "clear" ? 1 : 0);
  return (
    <>
      <Label className="flex min-w-0 items-center gap-2 px-2 pt-1 pb-1.5 text-xs text-neutral-500">
        <StateGlyph glyph={found.glyph} still />
        <span className="truncate">
          {found.workspace.name} / {found.title ?? found.name}
        </span>
      </Label>
      {id !== at.current && (
        <Item onSelect={() => at.onOpen(id)} className={item}>
          <Icon d="M3 3.5h10v9H3zM3 6h10" />
          Open
        </Item>
      )}
      {at.current && id !== at.current && wide() && (
        <Item onSelect={() => at.onBeside(id)} className={item}>
          <Icon d="M2.5 3h11v10h-11zM8 3v10" />
          Open beside
        </Item>
      )}
      {found.session && (
        <Item onSelect={() => at.onSteps(id)} className={item}>
          <Icon d="M3 4h2M3 8h2M3 12h2M7 4h6M7 8h6M7 12h4" />
          Steps
        </Item>
      )}
      {id === at.current && !wide() && (
        <Item onSelect={at.onFind} className={item}>
          <Icon d="M2 3.5h10M2 7h5M2 10.5h3.5M12.5 12l1.75 1.75M13.25 10a2.75 2.75 0 1 1-5.5 0 2.75 2.75 0 0 1 5.5 0" />
          Find in scrollback
        </Item>
      )}
      <Item onSelect={() => at.onChanges(found.workspace.id)} className={item}>
        <Icon d="M8 2.5v6M5 5.5h6M5 12.5h6" />
        Changes in {found.workspace.name}
      </Item>
      {(kind || sweep) && <Separator className="my-1 h-px bg-neutral-800" />}
      {kind === "end" && (
        <Item onSelect={() => at.end(id)} className={`${item} text-red-400 data-highlighted:bg-red-400/10 data-highlighted:text-red-300`}>
          <Icon d="M8 2.5v5M4.8 4.6a4.8 4.8 0 1 0 6.4 0" />
          End {found.name}…
        </Item>
      )}
      {kind === "clear" && (
        <Item onSelect={() => at.end(id)} className={item}>
          <Icon d="M3.5 4.5h9M6.5 4.5V3h3v1.5M4.5 4.5l.7 8.5h5.6l.7-8.5" />
          Clear
        </Item>
      )}
      {sweep && (
        <Item onSelect={() => at.clear(rest)} className={item}>
          <Icon d="M11.5 2l-3 6M4 8h7l1 5.5H3zM6 10.5v3M9 10.5v3" />
          Clear {rest.length} ended in {found.workspace.name}
        </Item>
      )}
    </>
  );
}

function Icon({ d }: { d: string }) {
  return (
    <svg viewBox="0 0 16 16" className="size-3.5 shrink-0 text-neutral-500" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d={d} />
    </svg>
  );
}

/**
 * Terminal `id`'s row, which opens its menu on a right-click or a long press. Outside
 * `TerminalActions`, or for a terminal no longer listed, the row is shown as it is.
 */
export function TerminalMenu({ id, children }: { id: string; children: ReactElement }) {
  const at = useContext(Terminals);
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

/** The same menu from a button, for the terminal's header: the way in on a touch screen. */
export function TerminalMenuButton({ id }: { id: string }) {
  const at = useContext(Terminals);
  const focus = useReturnFocus();
  const found = at && locate(at.machines, id);
  if (!at || !found) return null;
  return (
    <DropdownMenu.Root modal={false}>
      <DropdownMenu.Trigger
        aria-label="More"
        title="More: open beside, steps, changes, end"
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

/** The ending of terminals, inside `TerminalActions`. */
export function useTerminalActions(): Pick<Context, "end" | "clear"> | undefined {
  return useContext(Terminals);
}

/** Asks before ending a terminal whose child still runs, saying what is lost. */
function EndDialog({ id, found, onDone }: { id: string; found: Located; onDone: () => void }) {
  const end = useMutation({ mutationFn: () => closeTerminal(id), onSuccess: onDone });
  const what = !found.session
    ? "Whatever runs in it is hung up and the terminal closes."
    : found.glyph === "working"
      ? "It is in the middle of a turn: Claude Code is hung up and the turn stops where it is."
      : found.glyph === "waiting"
        ? "It is waiting for your answer: Claude Code is hung up without one."
        : "Claude Code is hung up and its terminal closes.";
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
          {end.error.message}
        </p>
      )}
      <div className="mt-4 flex justify-end gap-2">
        <Dialog.Close className={button}>Cancel</Dialog.Close>
        <button
          type="button"
          onClick={() => end.mutate()}
          disabled={end.isPending}
          className="rounded bg-red-400 px-3 py-0.5 text-xs font-semibold text-neutral-950 hover:bg-red-300 disabled:opacity-50"
        >
          End
        </button>
      </div>
    </Modal>
  );
}
