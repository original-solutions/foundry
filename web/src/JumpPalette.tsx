import { useMutation } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { Dialog } from "radix-ui";
import { useEffect, useMemo, useRef, useState } from "react";
import { startShell } from "./api";
import type { MachineView } from "./fleet";
import { destinations, newWorkspace, search, type Destination } from "./jump";
import { StateGlyph } from "./StateGlyph";

/** The key that opens the palette, as this platform writes it. */
export const jumpKey = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent) ? "⌘K" : "Ctrl K";

interface Props {
  machines: MachineView[];
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Opens the new-workspace dialog with `name` typed in. */
  onCreate: (name: string) => void;
  /** The open terminal, which a pick can be opened beside. */
  current?: { terminal: string; title: string };
  /** When true, every pick opens beside `current`; otherwise only a Shift pick does. */
  besideOnly?: boolean;
  /** Shows `terminal` beside `current`. */
  onBeside?: (terminal: string) => void;
}

/**
 * Jump to any session or shell in the fleet, or start a new shell in any workspace, by typing a
 * few words of its workspace, name, state, project, branch or machine. What was typed can also
 * name a new workspace. With a terminal open, Shift+Enter (or a Shift click) opens the pick beside
 * it instead; opened as "Open beside", every pick does.
 */
export function JumpPalette({ machines, open, onOpenChange, onCreate, current, besideOnly = false, onBeside }: Props) {
  // After a pick the opened terminal has the keyboard, so focus does not go back where it was.
  const picked = useRef(false);
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
          className="fixed inset-x-2 top-2 z-50 mx-auto flex max-h-[80dvh] max-w-xl flex-col overflow-hidden rounded-xl border border-neutral-700/70 bg-neutral-900 text-sm text-neutral-100 shadow-2xl shadow-black/60 sm:top-[12vh]"
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
            placeholder={besideOnly && current ? `Open beside ${current.title}…` : "Workspace, model, state, machine…"}
            besideOnly={besideOnly && !!current}
            onBeside={current ? onBeside : undefined}
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
}: {
  machines: MachineView[];
  close: () => void;
  onCreate: (name: string) => void;
  current?: string;
  placeholder: string;
  besideOnly: boolean;
  onBeside?: (terminal: string) => void;
}) {
  const navigate = useNavigate();
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const all = useMemo(() => destinations(machines).filter((d) => !(besideOnly && d.kind === "terminal" && d.terminal === current)), [machines, besideOnly, current]);
  const found = useMemo(() => search(all, query), [all, query]);
  const shown = useMemo(() => (besideOnly ? found : [...found, ...newWorkspace(query)]), [found, query, besideOnly]);
  const at = Math.min(active, Math.max(shown.length - 1, 0));
  const list = useRef<HTMLUListElement>(null);
  const shell = useMutation({ mutationFn: startShell });
  const many = machines.length > 1;

  useEffect(() => {
    list.current?.querySelector('[aria-selected="true"]')?.scrollIntoView?.({ block: "nearest" });
  }, [at, shown]);

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
    shell.mutate(d.workspace, { onSuccess: show });
  };

  const keys = (event: React.KeyboardEvent) => {
    const move = { ArrowDown: 1, ArrowUp: -1 }[event.key];
    if (move) {
      event.preventDefault();
      if (shown.length) setActive((at + move + shown.length) % shown.length);
    } else if (event.key === "Enter") {
      event.preventDefault();
      go(shown[at], event.shiftKey);
    }
  };

  const optionId = (d: Destination) => `jump-${d.key}`;
  return (
    <>
      <div className="flex items-center gap-3 border-b border-neutral-800 px-4">
        <SearchIcon className="size-4 shrink-0 text-neutral-500" />
        <input
          role="combobox"
          aria-expanded="true"
          aria-controls="jump-list"
          aria-autocomplete="list"
          aria-activedescendant={shown[at] ? optionId(shown[at]) : undefined}
          aria-label="Jump to"
          autoFocus
          value={query}
          onChange={(event) => {
            setQuery(event.target.value);
            setActive(0);
          }}
          onKeyDown={keys}
          placeholder={placeholder}
          className="h-12 min-w-0 flex-1 bg-transparent text-base text-neutral-100 outline-none placeholder:text-neutral-500"
        />
        <kbd className="hidden rounded border border-neutral-700 px-1.5 py-0.5 text-[10px] text-neutral-400 sm:block">Esc</kbd>
      </div>
      <ul id="jump-list" ref={list} role="listbox" aria-label="Sessions and shells" className="min-h-0 flex-1 overflow-y-auto p-1.5">
        {shown.map((d, i) => (
          <li
            key={d.key}
            id={optionId(d)}
            role="option"
            aria-selected={i === at}
            onMouseMove={() => i !== at && setActive(i)}
            onClick={(event) => go(d, event.shiftKey)}
            className={`flex cursor-pointer items-center gap-3 rounded-lg px-3 py-2.5 sm:py-2 ${i === at ? "bg-neutral-800 text-neutral-50" : "text-neutral-200"}`}
          >
            {d.kind === "terminal" ? (
              <span className="flex w-4 justify-center">
                <StateGlyph glyph={d.glyph} />
              </span>
            ) : (
              <PlusIcon className="size-4 shrink-0 text-neutral-500" />
            )}
            <span className="min-w-0 flex-1 truncate">{d.title}</span>
            {d.kind === "terminal" && <span className="shrink-0 text-xs text-neutral-400">{d.label}</span>}
            {many && <span className="hidden max-w-32 shrink-0 truncate text-xs text-neutral-500 sm:inline">{d.machine}</span>}
          </li>
        ))}
      </ul>
      {found.length === 0 && <p className="px-4 pb-4 pt-1 text-neutral-500">Nothing matches “{query.trim()}”. ↵ makes it a new workspace.</p>}
      {shell.isPending && <p className="border-t border-neutral-800 px-4 py-2 text-xs text-neutral-400">Starting a shell…</p>}
      {shell.isError && (
        <p role="alert" className="border-t border-neutral-800 px-4 py-2 text-xs text-red-400">
          {shell.error.message}
        </p>
      )}
      <div className="hidden gap-4 border-t border-neutral-800 px-4 py-2 text-[11px] text-neutral-500 sm:flex">
        <span>↑↓ move</span>
        <span>↵ {besideOnly ? "open beside" : "open"}</span>
        {onBeside && !besideOnly && <span>⇧↵ open beside</span>}
        <span className="ml-auto">{all.filter((d) => d.kind === "terminal").length} terminals</span>
      </div>
    </>
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
