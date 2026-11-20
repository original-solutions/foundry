import { Link } from "@tanstack/react-router";
import { DropdownMenu } from "radix-ui";
import { useEffect, useRef } from "react";
import { rows, type MachineView, type WorkspaceView } from "./fleet";
import { Named } from "./Now";
import { glyphText, StateGlyph } from "./StateGlyph";
import { TerminalMenu, useTerminalActions } from "./TerminalMenu";

interface Props {
  /** The workspace of the terminal with the keyboard, and the machine it is on. */
  workspace?: WorkspaceView;
  machine?: MachineView;
  /** The terminal shown now, with the keyboard. */
  active?: string;
  /** The terminal shown beside it in a split. */
  beside?: string;
  /** Tabs in the background whose state changed since they were last shown. */
  changed: Set<string>;
}

/**
 * The open workspace's terminals, its sessions then its shells as the sidebar lists them, one tab
 * each with its live state, so its agent and its shells are one click apart. Another workspace's
 * terminals are its own tabs, shown when one of them is open. A tab whose state changed while it
 * was in the background says its new state until it is shown. The + at the end starts a session or
 * a shell in the workspace, on the machine serving the UI only. On a narrow screen the strip shows
 * only once there is a second tab to switch to.
 */
export function TabStrip({ workspace, machine, active, beside, changed }: Props) {
  const strip = useRef<HTMLDivElement>(null);
  const actions = useTerminalActions();
  useEffect(() => {
    strip.current?.querySelector('[aria-selected="true"]')?.scrollIntoView?.({ block: "nearest", inline: "nearest" });
  }, [active, workspace?.id]);
  if (!workspace || !machine) return null;
  const list = rows(workspace);
  const canAdd = actions !== undefined && actions.here === machine.id;
  return (
    <div
      ref={strip}
      role="tablist"
      aria-label={`Terminals in ${workspace.name}`}
      className={`${list.length < 2 ? "hidden md:flex" : "flex"} h-9 shrink-0 items-stretch overflow-x-auto border-b border-neutral-800 bg-neutral-950 [scrollbar-width:none]`}
    >
      {list.map((row) => {
        const current = row.terminal === active;
        const fresh = !current && changed.has(row.terminal);
        return (
          <TerminalMenu key={row.terminal} id={row.terminal}>
            <div
              role="presentation"
              className={`relative flex min-w-0 max-w-[60vw] shrink-0 border-r border-neutral-800 md:max-w-72 ${
                current ? "bg-neutral-900 text-neutral-100" : "text-neutral-400 hover:bg-neutral-900/60 hover:text-neutral-200"
              }`}
            >
              {current && <span aria-hidden="true" className="absolute inset-x-0 top-0 h-0.5 bg-sky-400" />}
              {row.terminal === beside && <span aria-hidden="true" className="absolute inset-x-0 top-0 h-0.5 bg-sky-400/35" />}
              <Link
                to="/terminals/$terminalId"
                params={{ terminalId: row.terminal }}
                role="tab"
                aria-selected={current}
                data-changed={fresh ? "true" : undefined}
                data-beside={row.terminal === beside ? "true" : undefined}
                title={`${workspace.name} / ${row.title ?? row.name}: ${row.label}`}
                className="flex min-w-0 flex-1 items-center gap-2 px-3 py-1 text-xs"
              >
                <StateGlyph glyph={row.glyph} />
                <span className={`flex min-w-0 ${fresh ? "font-semibold text-neutral-100" : ""}`}>
                  <Named name={row.name} title={row.title} />
                </span>
                {fresh && <span className={`shrink-0 font-medium ${glyphText[row.glyph]}`}>{row.label}</span>}
              </Link>
            </div>
          </TerminalMenu>
        );
      })}
      {canAdd && (
        <DropdownMenu.Root modal={false}>
          <DropdownMenu.Trigger
            aria-label={`New in ${workspace.name}`}
            title="New session or shell in this workspace"
            className="grid w-9 shrink-0 place-items-center text-neutral-500 hover:bg-neutral-900 hover:text-neutral-100 data-[state=open]:bg-neutral-900 data-[state=open]:text-neutral-100"
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

const entry = "flex cursor-default select-none items-center gap-2.5 rounded-md px-2 py-1.5 outline-none data-highlighted:bg-neutral-800 data-highlighted:text-neutral-50";
