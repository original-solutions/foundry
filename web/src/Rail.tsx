import { Link } from "@tanstack/react-router";
import { useState, type KeyboardEvent, type ReactNode } from "react";
import { SparkIcon } from "./FirstPrompt";
import { jumpKey, SearchIcon } from "./JumpPalette";
import { Mark } from "./Mark";
import { rows, type MachineView } from "./fleet";
import { railKey } from "./collapsed";
import { PanelIcon, stripOf } from "./Sidebar";
import { StateGlyph } from "./StateGlyph";
import { TerminalMenu } from "./TerminalMenu";
import { ThemeButton } from "./ThemeButton";

const statusDot = { online: "bg-emerald-400", offline: "bg-red-400", unknown: "bg-neutral-500" };

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
 * list beside a terminal, or a terminal wanting the room. It still shows every terminal's state:
 * each workspace is a column of glyphs (the most urgent first, six at most, the open one always),
 * each a link to its terminal named by workspace, terminal and state, and the open terminal's
 * workspace stands out. The board, jump, a new task and the bell keep a button each. The terminals
 * are one Tab stop that the arrows walk, as in the full list.
 */
export function Rail({ machines, selected, onExpand, onJump, onCreate, alerts }: Props) {
  const [focused, setFocused] = useState<string>();
  const listed = machines.flatMap((m) => m.workspaces.flatMap((w) => stripOf(rows(w), selected).map((r) => r.terminal)));
  const stop = [focused, selected].find((id) => id !== undefined && listed.includes(id)) ?? listed[0];

  const keys = (event: KeyboardEvent<HTMLElement>) => {
    const here = event.target instanceof HTMLElement ? event.target.closest<HTMLElement>("[data-rail]") : null;
    if (!here || event.metaKey || event.ctrlKey || event.altKey) return;
    const all = [...event.currentTarget.querySelectorAll<HTMLElement>("[data-rail]")];
    const at = all.indexOf(here);
    const to = { ArrowDown: all[at + 1], ArrowUp: all[at - 1], Home: all[0], End: all.at(-1) }[event.key];
    if (!to) return;
    event.preventDefault();
    to.focus();
  };

  return (
    <nav aria-label="Workspaces" onKeyDown={keys} className="flex h-full w-12 shrink-0 flex-col items-center gap-1 overflow-y-auto bg-neutral-900 py-3 text-neutral-400 [scrollbar-width:none]">
      <Link to="/" aria-label="The board" title="The board" className="grid size-8 place-items-center rounded-md hover:bg-neutral-800">
        <Mark className="size-4 text-emerald-400" />
      </Link>
      <RailButton label="Expand sidebar" title={`Show every workspace and terminal by name (${railKey})`} onClick={onExpand}>
        <PanelIcon className="size-4" />
      </RailButton>
      <RailButton label="Jump to" title={`Jump to (${jumpKey})`} onClick={onJump}>
        <SearchIcon className="size-4" />
      </RailButton>
      <RailButton label="New task" title="Give Claude a new task" onClick={onCreate}>
        <SparkIcon className="size-4 text-emerald-400" />
      </RailButton>
      {machines.map((machine) => (
        <section key={machine.id} aria-label={machine.name} className="mt-2 flex w-full flex-col items-center gap-3 border-t border-neutral-800 pt-3">
          <Link
            to="/machines"
            aria-label={`${machine.name} ${machine.status}`}
            title={`${machine.name}: ${machine.status}`}
            className="grid size-6 place-items-center rounded hover:bg-neutral-800"
          >
            <span aria-hidden="true" className={`size-1.5 rounded-full ${statusDot[machine.status]}`} />
          </Link>
          {machine.workspaces.map((workspace) => {
            const list = rows(workspace);
            const shown = stripOf(list, selected);
            const open = list.some((r) => r.terminal === selected);
            const rest = list.length - shown.length;
            return (
              <div
                key={workspace.id}
                role="group"
                aria-label={workspace.name}
                data-open={open || undefined}
                className={`flex flex-col items-center gap-px rounded-lg p-0.5 ${open ? "bg-neutral-800/70 ring-1 ring-neutral-700" : ""}`}
              >
                {shown.map((row) => {
                  const name = `${workspace.name} / ${row.name}: ${row.label}`;
                  const current = row.terminal === selected;
                  return (
                    <TerminalMenu key={row.terminal} id={row.terminal}>
                      <Link
                        to="/terminals/$terminalId"
                        params={{ terminalId: row.terminal }}
                        aria-label={name}
                        title={row.title ? `${name}\n${row.title}` : name}
                        aria-current={current ? "true" : undefined}
                        data-rail=""
                        tabIndex={row.terminal === stop ? 0 : -1}
                        onFocus={() => setFocused(row.terminal)}
                        className={`grid size-7 place-items-center rounded-md hover:bg-neutral-700 ${current ? "bg-neutral-700" : ""}`}
                      >
                        <StateGlyph glyph={row.glyph} />
                      </Link>
                    </TerminalMenu>
                  );
                })}
                {rest > 0 && (
                  <span title={`${rest} more in ${workspace.name}`} className="pb-0.5 text-[10px] leading-3 text-neutral-500">
                    +{rest}
                  </span>
                )}
                {list.length === 0 && <span title={`${workspace.name}: no terminals`} aria-hidden="true" className="m-2.5 size-2 rounded-full border border-neutral-600" />}
              </div>
            );
          })}
        </section>
      ))}
      <div className="mt-auto flex flex-col items-center gap-1 pt-3">
        {alerts}
        <ThemeButton />
      </div>
    </nav>
  );
}

function RailButton({ label, title, onClick, children }: { label: string; title: string; onClick: () => void; children: ReactNode }) {
  return (
    <button type="button" aria-label={label} title={title} onClick={onClick} className="grid size-8 place-items-center rounded-md hover:bg-neutral-800 hover:text-neutral-200">
      {children}
    </button>
  );
}
