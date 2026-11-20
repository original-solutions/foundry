import { Link } from "@tanstack/react-router";
import { DropdownMenu } from "radix-ui";
import { useEffect, useRef } from "react";
import { rows, type MachineView, type WorkspaceView } from "./fleet";
import { Named } from "./Now";
import { glyphText, StateGlyph } from "./StateGlyph";
import { TerminalMenu, useTerminalActions } from "./TerminalMenu";

interface Props {
  /** The workspace of the terminal this bar heads, and the machine it is on. */
  workspace: WorkspaceView;
  machine: MachineView;
  /** The terminal this bar heads. */
  active: string;
  /** Tabs in the background whose state changed since they were last shown. */
  changed: Set<string>;
  /** The bar is half a split: two or more tabs take their own row at every width. */
  apart?: boolean;
}

/**
 * The tabs in a terminal's bar: its workspace's terminals, its sessions then its shells as the
 * sidebar lists them, one tab each with its live state, so its agent and its shells are one click
 * apart. The open tab says its state; a tab whose state changed while it was in the background
 * says its new state until it is shown. The + at the end starts a session or a shell in the
 * workspace, on the machine serving the UI only. In a split each side's bar has its own tabs, as
 * an editor's two groups do. On a narrow screen, or in a half of a split, two or more tabs take a
 * row of their own under the bar; one sits in it.
 */
export function TabStrip({ workspace, machine, active, changed, apart = false }: Props) {
  const strip = useRef<HTMLDivElement>(null);
  const actions = useTerminalActions();
  useEffect(() => {
    strip.current?.querySelector('[aria-selected="true"]')?.scrollIntoView?.({ block: "nearest", inline: "nearest" });
  }, [active, workspace.id]);
  const list = rows(workspace);
  const canAdd = actions !== undefined && actions.here === machine.id;
  return (
    <div
      ref={strip}
      role="tablist"
      aria-label={`Terminals in ${workspace.name}`}
      className={`flex min-w-0 flex-1 items-center gap-0.5 overflow-x-auto [scrollbar-width:none] ${list.length < 2 ? "" : apart ? ownRow : narrowRow}`}
    >
      {list.map((row) => {
        const current = row.terminal === active;
        const fresh = !current && changed.has(row.terminal);
        return (
          <TerminalMenu key={row.terminal} id={row.terminal}>
            <Link
              to="/terminals/$terminalId"
              params={{ terminalId: row.terminal }}
              role="tab"
              aria-selected={current}
              data-changed={fresh ? "true" : undefined}
              title={`${workspace.name} / ${row.title ?? row.name}: ${row.label}`}
              className={`flex h-7 min-w-0 max-w-[60vw] shrink-0 items-center gap-2 rounded-md px-2 text-xs md:max-w-72 ${
                current ? "bg-neutral-800 text-neutral-100 ring-1 ring-neutral-700" : "text-neutral-400 hover:bg-neutral-800/60 hover:text-neutral-200"
              }`}
            >
              <StateGlyph glyph={row.glyph} />
              <span className={`flex min-w-0 ${fresh ? "font-semibold text-neutral-100" : ""}`}>
                <Named name={row.name} title={row.title} />
              </span>
              {(current || fresh) && <span className={`shrink-0 font-medium ${glyphText[row.glyph]}`}>{row.label}</span>}
            </Link>
          </TerminalMenu>
        );
      })}
      {canAdd && (
        <DropdownMenu.Root modal={false}>
          <DropdownMenu.Trigger
            aria-label={`New in ${workspace.name}`}
            title="New session or shell in this workspace"
            className="grid size-7 shrink-0 place-items-center rounded-md text-neutral-500 hover:bg-neutral-800 hover:text-neutral-100 data-[state=open]:bg-neutral-800 data-[state=open]:text-neutral-100"
          >
            <svg viewBox="0 0 16 16" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="1.6" aria-hidden="true">
              <path d="M8 3v10M3 8h10" strokeLinecap="round" />
            </svg>
          </DropdownMenu.Trigger>
          <DropdownMenu.Portal>
            <DropdownMenu.Content
              align="start"
              sideOffset={4}
              collisionPadding={8}
              className="z-50 min-w-44 overflow-hidden rounded-lg border border-neutral-700 bg-neutral-900 p-1 text-sm text-neutral-200 shadow-xl shadow-black/40"
            >
              <DropdownMenu.Item onSelect={() => actions.onNewSession(workspace.id, workspace.name)} className={entry}>
                New session…
              </DropdownMenu.Item>
              <DropdownMenu.Item onSelect={() => actions.shell(workspace.id)} className={entry}>
                New shell
              </DropdownMenu.Item>
            </DropdownMenu.Content>
          </DropdownMenu.Portal>
        </DropdownMenu.Root>
      )}
    </div>
  );
}

/** Its own row under the bar, at every width, and below `md` only. */
const ownRow = "order-last -mx-3 h-10 basis-full border-t border-neutral-800 px-2";
const narrowRow = "max-md:order-last max-md:-mx-3 max-md:h-10 max-md:basis-full max-md:border-t max-md:border-neutral-800 max-md:px-2";

const entry = "flex cursor-default select-none items-center gap-2.5 rounded-md px-2 py-1.5 outline-none data-highlighted:bg-neutral-800 data-highlighted:text-neutral-50";
