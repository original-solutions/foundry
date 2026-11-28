import { Link } from "@tanstack/react-router";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { SparkIcon } from "./FirstPrompt";
import { jumpKey, SearchIcon } from "./JumpPalette";
import { FactoryIcon } from "./FactoryMark";
import { Mark } from "./Mark";
import { type MachineView } from "./fleet";
import { needing } from "./needing";
import { railKey } from "./collapsed";
import { PanelIcon } from "./Sidebar";
import { loadDocked, saveDocked, SessionList } from "./SessionList";
import { SettingsLink, ThemeButton } from "./ThemeButton";

interface Props {
  machines: MachineView[];
  /** The terminal open in the main pane. */
  selected?: string;
  /** Back to the full list. */
  onExpand: () => void;
  onJump: () => void;
  /** Opens the New task dialog. */
  onCreate: () => void;
  /** The alerts bell, at the foot. */
  alerts?: ReactNode;
}

/**
 * The sidebar collapsed to a strip as narrow as a button, for a window too narrow for the full
 * list beside a terminal, or a terminal wanting the room. It holds only buttons: the board, the
 * factory, the sessions, jump, a new task and the bell. Sessions carries how many need you and
 * opens every terminal as one list (`SessionList`), over the terminal until another terminal opens
 * (a pick, or from a row's menu), Escape is pressed or the terminal is clicked, or docked beside the rail to stay open.
 */
export function Rail({ machines, selected, onExpand, onJump, onCreate, alerts }: Props) {
  const [docked, setDocked] = useState(loadDocked);
  const [open, setOpen] = useState(docked);
  // The floating list is for the terminal it was opened on: opening another, by a pick or from a row's menu, closes it.
  const [openedOn, setOpenedOn] = useState(selected);
  const shown = open && (docked || openedOn === selected);
  // A docked list that shows by itself leaves the focus where it is; opened from its button, it takes it.
  const [asked, setAsked] = useState(false);
  const button = useRef<HTMLButtonElement>(null);
  const need = needing(machines).length;
  const flying = shown && !docked;

  useEffect(() => {
    if (!flying) return;
    const close = () => setOpen(false);
    const onKey = (event: globalThis.KeyboardEvent) => {
      if (event.key !== "Escape") return;
      close();
      button.current?.focus();
    };
    const onDown = (event: PointerEvent) => {
      if (event.target instanceof Element && event.target.closest("main")) close();
    };
    document.addEventListener("keydown", onKey);
    document.addEventListener("pointerdown", onDown);
    return () => {
      document.removeEventListener("keydown", onKey);
      document.removeEventListener("pointerdown", onDown);
    };
  }, [flying]);

  const dock = (on: boolean) => {
    saveDocked(on);
    setDocked(on);
    setOpenedOn(selected);
  };

  return (
    <>
      <nav aria-label="Workspaces" className="flex h-full w-12 shrink-0 flex-col items-center gap-1 overflow-y-auto border-r border-neutral-800 bg-neutral-900 py-3 text-neutral-400 [scrollbar-width:none]">
        <Link to="/" aria-label="The board" title="The board" className="grid size-8 place-items-center rounded-md hover:bg-neutral-800">
          <Mark className="size-4 text-emerald-400" />
        </Link>
        <Link to="/factory" aria-label="The factory" title="The factory" className="grid size-8 place-items-center rounded-md hover:bg-neutral-800 hover:text-neutral-200">
          <FactoryIcon className="size-4" />
        </Link>
        <span aria-hidden="true" className="my-1.5 h-px w-6 bg-neutral-800" />
        <button
          ref={button}
          type="button"
          aria-label={need > 0 ? `Sessions: ${need} need${need === 1 ? "s" : ""} you` : "Sessions"}
          aria-expanded={shown}
          title="Every session and shell, by state"
          onClick={() => {
            setAsked(true);
            setOpenedOn(selected);
            setOpen(!shown);
          }}
          className={`relative grid size-8 place-items-center rounded-md hover:bg-neutral-800 hover:text-neutral-200 ${shown ? "bg-neutral-800 text-neutral-200" : ""}`}
        >
          <SessionsIcon className="size-4" />
          {need > 0 && (
            <span aria-hidden="true" className="absolute -top-0.5 -right-1 min-w-4 rounded-full bg-amber-400 px-1 text-center text-[10px] leading-4 font-semibold text-neutral-950 tabular-nums">
              {need}
            </span>
          )}
        </button>
        <RailButton label="Jump to" title={`Jump to (${jumpKey})`} onClick={onJump}>
          <SearchIcon className="size-4" />
        </RailButton>
        <RailButton label="New task" title="Give Claude a new task" onClick={onCreate}>
          <SparkIcon className="size-4 text-emerald-400" />
        </RailButton>
        <div className="mt-auto flex flex-col items-center gap-1 pt-3">
          <RailButton label="Expand sidebar" title={`Show every workspace and terminal by name (${railKey})`} onClick={onExpand}>
            <PanelIcon className="size-4" />
          </RailButton>
          {alerts}
          <ThemeButton />
          <SettingsLink />
        </div>
      </nav>
      {shown && <SessionList machines={machines} selected={selected} docked={docked} focus={asked} onDock={dock} onPick={() => !docked && setOpen(false)} />}
    </>
  );
}

/** Three lines with a dot before each: a list of sessions. */
function SessionsIcon({ className }: { className?: string }) {
  return (
    <svg aria-hidden="true" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" className={className}>
      <path d="M6 4h7.5M6 8h7.5M6 12h7.5" />
      <path d="M2.5 4h.01M2.5 8h.01M2.5 12h.01" strokeWidth="2" />
    </svg>
  );
}

function RailButton({ label, title, onClick, children }: { label: string; title: string; onClick: () => void; children: ReactNode }) {
  return (
    <button type="button" aria-label={label} title={title} onClick={onClick} className="grid size-8 place-items-center rounded-md hover:bg-neutral-800 hover:text-neutral-200">
      {children}
    </button>
  );
}
