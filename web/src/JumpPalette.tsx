import { useMutation } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { Dialog } from "radix-ui";
import { Fragment, useEffect, useMemo, useRef, useState, type MutableRefObject } from "react";
import { startShell } from "./api";
import { couldNot } from "./labels";
import { locate, type MachineView } from "./fleet";
import { viewLook } from "./Modal";
import { destinations, newWorkspace, search, type Command, type Destination } from "./jump";
import { Peek, useWide } from "./Peek";
import { StateGlyph } from "./StateGlyph";
import { actsFor, Icon, useTerminalActions, type Act } from "./TerminalMenu";

/** The key that opens the palette, as this platform writes it. */
export const jumpKey = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent) ? "⌘K" : "Ctrl K";

interface Props {
  machines: MachineView[];
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Opens the new-task dialog with `name` as its task. */
  onCreate: (name: string) => void;
  /** The open terminal, which a pick can be opened beside. */
  current?: { terminal: string; title: string };
  /** When true, every pick opens beside `current`; otherwise only a Shift pick does. */
  besideOnly?: boolean;
  /** Shows `terminal` beside `current`. */
  onBeside?: (terminal: string) => void;
  /** The app's views and actions, offered after the terminals; not when every pick opens beside. */
  commands?: Command[];
}

/**
 * Jump to any session or shell in the fleet, or start a new shell in any workspace, by typing a
 * few words of its workspace, name, state, project, branch or machine. What was typed can also
 * name a new workspace. The app's views and actions are here too, after the terminals, and first
 * once a word names one. With a terminal open, Shift+Enter (or a Shift click) opens the pick beside
 * it instead; opened as "Open beside", every pick does.
 *
 * Tab on a terminal (or its row's actions button) lists what can be done with it, as its
 * right-click menu does; typing narrows that list and Enter does the one picked. Escape, Shift+Tab
 * or Backspace in an empty box goes back to the list as it was.
 */
export function JumpPalette({ machines, open, onOpenChange, onCreate, current, besideOnly = false, onBeside, commands = [] }: Props) {
  // After a pick the opened terminal has the keyboard, so focus does not go back where it was.
  const picked = useRef(false);
  // A wide screen previews the highlighted row beside the list.
  const wide = useWide();
  /** Leaves a terminal's actions for the list, answering whether there were any to leave. */
  const back = useRef<() => boolean>(() => false);
  const change = (next: boolean) => {
    if (next) picked.current = false;
    onOpenChange(next);
  };
  return (
    <Dialog.Root open={open} onOpenChange={change}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-black/60 backdrop-blur-[2px]" />
        <Dialog.Content
          aria-describedby={undefined}
          onCloseAutoFocus={(event) => picked.current && event.preventDefault()}
          onEscapeKeyDown={(event) => back.current() && event.preventDefault()}
          className={`fixed inset-x-2 top-2 z-50 mx-auto flex max-h-[80dvh] ${wide ? "h-[min(36rem,80dvh)] max-w-4xl" : "max-w-xl"} flex-col overflow-hidden rounded-xl border border-neutral-700/70 bg-neutral-900 text-sm text-neutral-100 shadow-2xl shadow-black/60 sm:top-[12vh]`}
        >
          <Dialog.Title className="sr-only">{besideOnly && current ? `Open beside ${current.title}` : "Jump to"}</Dialog.Title>
          <Panel
            machines={machines}
            close={() => {
              picked.current = true;
              onOpenChange(false);
            }}
            onCreate={onCreate}
            current={current?.terminal}
            placeholder={besideOnly && current ? `Open beside ${current.title}…` : "A session, a view or an action…"}
            besideOnly={besideOnly && !!current}
            onBeside={current ? onBeside : undefined}
            backRef={back}
            wide={wide}
            commands={besideOnly && current ? [] : commands}
          />
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

function Panel({
  machines,
  close,
  onCreate,
  current,
  placeholder,
  besideOnly,
  onBeside,
  backRef,
  wide,
  commands,
}: {
  machines: MachineView[];
  close: () => void;
  onCreate: (name: string) => void;
  current?: string;
  placeholder: string;
  besideOnly: boolean;
  onBeside?: (terminal: string) => void;
  /** Set to leave the actions for the list, as Escape does. */
  backRef: MutableRefObject<() => boolean>;
  /** Shows the highlighted row's preview beside the list. */
  wide: boolean;
  commands: Command[];
}) {
  const navigate = useNavigate();
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  /** The terminal whose actions are listed, and the list as it was before. */
  const [acting, setActing] = useState<{ terminal: string; query: string; active: number }>();
  const terminals = useTerminalActions();
  const actable = !!terminals && !besideOnly;
  const target = acting && terminals ? locate(machines, acting.terminal) : undefined;
  const all = useMemo(
    () => destinations(machines, commands).filter((d) => !(besideOnly && d.kind === "terminal" && d.terminal === current)),
    [machines, commands, besideOnly, current],
  );
  const found = useMemo(() => search(all, query), [all, query]);
  // With nothing typed the views sit in a strip over the list, a tap away; typing finds them as rows.
  const views = commands.filter((c) => c.group === "Go to");
  const strip = views.length > 0 && query.trim() === "" && !acting;
  const shown = useMemo(
    () => (besideOnly ? found : [...(strip ? found.filter((d) => !(d.kind === "command" && d.command.group === "Go to")) : found), ...newWorkspace(query)]),
    [found, query, besideOnly, strip],
  );
  // Once views or actions are listed, each kind of row is headed, so they read apart from the fleet's terminals.
  const sorted = shown.some((d) => d.kind === "command");
  const acts = target && terminals ? actsMatching(actsFor(acting!.terminal, terminals, target), query) : undefined;
  const count = acts ? acts.length : shown.length;
  const at = Math.min(active, Math.max(count - 1, 0));
  const list = useRef<HTMLUListElement>(null);
  const shell = useMutation({ mutationFn: startShell });
  const many = machines.length > 1;

  useEffect(() => {
    list.current?.querySelector('[aria-selected="true"]')?.scrollIntoView?.({ block: "nearest" });
  }, [at, shown, acting]);

  const act = (terminal: string, from: number) => {
    setActing({ terminal, query, active: from });
    setQuery("");
    setActive(0);
  };
  const leave = () => {
    if (!acting) return false;
    setQuery(acting.query);
    setActive(acting.active);
    setActing(undefined);
    return true;
  };
  useEffect(() => {
    backRef.current = leave;
  });

  const go = (d: Destination | undefined, shift = false) => {
    if (!d || shell.isPending) return;
    const show = (terminalId: string) => {
      close();
      if (onBeside && (besideOnly || shift) && terminalId !== current) onBeside(terminalId);
      else void navigate({ to: "/terminals/$terminalId", params: { terminalId } });
    };
    if (d.kind === "terminal") return show(d.terminal);
    if (d.kind === "workspace") {
      close();
      onCreate(d.name);
      return;
    }
    if (d.kind === "command") {
      close();
      d.command.run();
      return;
    }
    shell.mutate(d.workspace, { onSuccess: show });
  };
  const run = (a: Act | undefined) => {
    if (!a) return;
    close();
    a.run();
  };

  const keys = (event: React.KeyboardEvent<HTMLInputElement>) => {
    const move = { ArrowDown: 1, ArrowUp: -1 }[event.key];
    if (move) {
      event.preventDefault();
      if (count) setActive((at + move + count) % count);
    } else if (event.key === "Enter") {
      event.preventDefault();
      if (acts) run(acts[at]);
      else go(shown[at], event.shiftKey);
    } else if (event.key === "Tab" && acts) {
      event.preventDefault();
      if (event.shiftKey) leave();
    } else if (event.key === "Tab" && !event.shiftKey && actable && shown[at]?.kind === "terminal") {
      event.preventDefault();
      act(shown[at].terminal, at);
    } else if (event.key === "Backspace" && acts && event.currentTarget.value === "") {
      event.preventDefault();
      leave();
    }
  };

  const title = target && `${target.workspace.name} / ${target.name}`;
  const label = title ? `Actions for ${title}` : "Jump to";
  const optionId = (key: string) => `jump-${key}`;
  const picked = acts ? acts[at]?.key : shown[at]?.key;
  return (
    <>
      <div className={`flex items-center gap-3 border-b border-neutral-800 ${target ? "pl-2.5 pr-4" : "px-4"}`}>
        {target ? (
          <button
            type="button"
            onClick={leave}
            title="Back to the list (Esc)"
            className="flex min-w-0 max-w-[55%] shrink-0 items-center gap-2 rounded-md border border-neutral-700 bg-neutral-800 py-1 pr-2 pl-1.5 text-xs text-neutral-100 hover:border-neutral-500"
          >
            <svg viewBox="0 0 16 16" className="size-3 shrink-0 text-neutral-400" fill="none" stroke="currentColor" strokeWidth="1.75" aria-hidden="true">
              <path d="M10 3.5L5.5 8l4.5 4.5" strokeLinecap="round" strokeLinejoin="round" />
            </svg>
            <StateGlyph glyph={target.glyph} />
            <span className="truncate">{target.title ? `${title} ${target.title}` : title}</span>
          </button>
        ) : (
          <SearchIcon className="size-4 shrink-0 text-neutral-500" />
        )}
        <input
          role="combobox"
          aria-expanded="true"
          aria-controls="jump-list"
          aria-autocomplete="list"
          aria-activedescendant={picked ? optionId(picked) : undefined}
          aria-label={label}
          autoFocus
          value={query}
          onChange={(event) => {
            setQuery(event.target.value);
            setActive(0);
          }}
          onKeyDown={keys}
          placeholder={target ? "Do what…" : placeholder}
          className="h-12 min-w-0 flex-1 bg-transparent text-base text-neutral-100 outline-none placeholder:text-neutral-500"
        />
        <kbd className="hidden rounded border border-neutral-700 px-1.5 py-0.5 text-[10px] text-neutral-400 sm:block">Esc</kbd>
      </div>
      <div className="flex min-h-0 flex-1">
        <div className="flex min-h-0 min-w-0 flex-1 flex-col">
          {strip && (
            <nav aria-label="Views" className="flex shrink-0 gap-1.5 border-b border-neutral-800 px-2 py-2 sm:px-3">
              {views.map((c) => {
                const here = c.hint === "Here";
                return (
                  <button
                    key={c.key}
                    type="button"
                    aria-current={here ? "page" : undefined}
                    title={c.about}
                    onClick={() => {
                      close();
                      c.run();
                    }}
                    className={`flex min-w-0 flex-1 flex-col items-center gap-1 rounded-lg border px-1 py-2 text-[11px] font-medium transition-colors sm:flex-none sm:flex-row sm:gap-1.5 sm:rounded-full sm:px-3 sm:py-1 sm:text-xs [&_svg]:size-4 sm:[&_svg]:size-3.5 ${
                      here ? `border-transparent ${viewLook.on}` : `border-neutral-700 ${viewLook.off} hover:border-neutral-500 hover:bg-neutral-800`
                    }`}
                  >
                    {c.icon}
                    {c.title}
                  </button>
                );
              })}
            </nav>
          )}
          {acts ? (
            <ul id="jump-list" ref={list} role="listbox" aria-label={label} className="min-h-0 flex-1 overflow-y-auto p-1.5">
              {acts.map((a, i) => (
                <li
                  key={a.key}
                  id={optionId(a.key)}
                  role="option"
                  aria-selected={i === at}
                  onMouseMove={() => i !== at && setActive(i)}
                  onClick={() => run(a)}
                  className={`flex cursor-pointer items-center gap-3 rounded-lg px-3 py-2.5 sm:py-2 ${i > 0 && a.group !== acts[i - 1].group ? "mt-1.5" : ""} ${
                    a.danger ? (i === at ? "bg-red-400/10 text-red-300" : "text-red-400") : i === at ? "bg-neutral-800 text-neutral-50" : "text-neutral-200"
                  }`}
                >
                  <span className="flex w-4 justify-center">
                    <Icon d={a.icon} />
                  </span>
                  {a.label}
                </li>
              ))}
            </ul>
          ) : (
            <ul id="jump-list" ref={list} role="listbox" aria-label="Sessions, shells, views and actions" className="min-h-0 flex-1 overflow-y-auto p-1.5">
              {shown.map((d, i) => (
                <Fragment key={d.key}>
                  {sorted && section(d) !== undefined && section(d) !== section(shown[i - 1]) && (
                    <li role="presentation" aria-hidden="true" className="px-3 pt-3 pb-1 text-[10px] font-medium tracking-wider text-neutral-500 uppercase first:pt-1.5">
                      {section(d)}
                    </li>
                  )}
                  <li
                    id={optionId(d.key)}
                    role="option"
                    aria-selected={i === at}
                    onMouseMove={() => i !== at && setActive(i)}
                    onClick={(event) => go(d, event.shiftKey)}
                    className={`group flex cursor-pointer items-center gap-3 rounded-lg px-3 py-2.5 sm:py-2 ${i === at ? "bg-neutral-800 text-neutral-50" : "text-neutral-200"}`}
                  >
                    {d.kind === "terminal" ? (
                      <span className="flex w-4 justify-center">
                        <StateGlyph glyph={d.glyph} />
                      </span>
                    ) : d.kind === "command" ? (
                      <span className="flex w-4 justify-center text-neutral-400">{d.command.icon}</span>
                    ) : (
                      <PlusIcon className="size-4 shrink-0 text-neutral-500" />
                    )}
                    <span className="min-w-0 flex-1">
                      <span className="block truncate">{d.title}</span>
                      {d.kind === "terminal" && d.subtitle && <span className="block truncate text-xs text-neutral-500">{d.subtitle}</span>}
                      {d.kind === "command" && !wide && <span className="block truncate text-xs text-neutral-500">{d.command.about}</span>}
                    </span>
                    {d.kind === "terminal" && (
                      <span title={d.fullLabel} className="shrink-0 text-xs text-neutral-400">
                        {d.label}
                      </span>
                    )}
                    {d.kind === "command" && d.command.hint && (
                      <span className={`shrink-0 text-xs ${d.command.hint === "Here" ? "text-neutral-200" : "text-neutral-400"}`}>{d.command.hint}</span>
                    )}
                    {d.kind === "command" && d.command.keys && (
                      <kbd className="hidden shrink-0 rounded border border-neutral-700 px-1.5 py-0.5 font-sans text-[10px] text-neutral-400 sm:block">{d.command.keys}</kbd>
                    )}
                    {many && <span className="hidden max-w-32 shrink-0 truncate text-xs text-neutral-500 sm:inline">{d.machine}</span>}
                    {actable && d.kind === "terminal" && (
                      <button
                        type="button"
                        tabIndex={-1}
                        aria-label={`Actions for ${d.title}`}
                        title="What can be done with it (Tab)"
                        onClick={(event) => {
                          event.stopPropagation();
                          act(d.terminal, i);
                        }}
                        className={`-my-1 -mr-1.5 shrink-0 rounded p-1 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-100 ${i === at ? "" : "pointer-fine:invisible pointer-fine:group-hover:visible"}`}
                      >
                        <DotsIcon />
                      </button>
                    )}
                  </li>
                </Fragment>
              ))}
            </ul>
          )}
          {acts && acts.length === 0 && <p className="px-4 pb-4 pt-1 text-neutral-500">No action matches “{query.trim()}”. ⌫ goes back.</p>}
          {!acts && found.length === 0 && <p className="px-4 pb-4 pt-1 text-neutral-500">Nothing matches “{query.trim()}”. ↵ makes it a task for Claude.</p>}
        </div>
        {wide && <Peek at={acting ? all.find((d) => d.kind === "terminal" && d.terminal === acting.terminal) : shown[at]} machines={machines} />}
      </div>
      {shell.isPending && <p className="border-t border-neutral-800 px-4 py-2 text-xs text-neutral-400">Starting a shell…</p>}
      {shell.isError && (
        <p role="alert" className="border-t border-neutral-800 px-4 py-2 text-xs text-red-400">
          {couldNot("start a shell", shell.error)}
        </p>
      )}
      <div className="hidden gap-4 border-t border-neutral-800 px-4 py-2 text-[11px] text-neutral-500 sm:flex">
        <span>↑↓ move</span>
        {acts ? (
          <>
            <span>↵ do it</span>
            <span>esc back</span>
            <span title={target?.fullLabel} className="ml-auto">
              {target?.label}
            </span>
          </>
        ) : (
          <>
            <span>↵ {besideOnly ? "open beside" : "open"}</span>
            {onBeside && !besideOnly && <span>⇧↵ open beside</span>}
            {actable && <span>⇥ actions</span>}
            <span className="ml-auto">{all.filter((d) => d.kind === "terminal").length} terminals</span>
          </>
        )}
      </div>
    </>
  );
}

/** The heading a row is listed under, when the list holds more than terminals. */
function section(d: Destination | undefined): string | undefined {
  if (!d) return undefined;
  if (d.kind === "command") return d.command.group;
  if (d.kind === "terminal") return "Sessions and shells";
  if (d.kind === "shell") return "New shell";
  return undefined;
}

/** The acts whose label holds every word of `query`, ignoring case. */
function actsMatching(acts: Act[], query: string): Act[] {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  return acts.filter((a) => words.every((word) => a.label.toLowerCase().includes(word)));
}

function DotsIcon() {
  return (
    <svg viewBox="0 0 16 16" className="size-4" fill="currentColor" aria-hidden="true">
      <circle cx="3.5" cy="8" r="1.25" />
      <circle cx="8" cy="8" r="1.25" />
      <circle cx="12.5" cy="8" r="1.25" />
    </svg>
  );
}

export function SearchIcon({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 16 16" className={className} fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
      <circle cx="7" cy="7" r="4.5" />
      <path d="M10.5 10.5L14 14" strokeLinecap="round" />
    </svg>
  );
}

function PlusIcon({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 16 16" className={className} fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
      <path d="M8 3v10M3 8h10" strokeLinecap="round" />
    </svg>
  );
}
