import { Link } from "@tanstack/react-router";
import { useEffect, useRef } from "react";
import { locate, type MachineView } from "./fleet";
import { glyphText, StateGlyph } from "./StateGlyph";

interface Props {
  machines: MachineView[];
  tabs: string[];
  /** The terminal shown now, with the keyboard. */
  active?: string;
  /** The terminal shown beside it in a split. */
  beside?: string;
  /** Tabs in the background whose state changed since they were last shown. */
  changed: Set<string>;
  onClose: (id: string) => void;
}

/**
 * The terminals opened in this browser, one tab each with its live state, so switching between
 * agents is one click. A tab whose state changed while it was in the background says its new
 * state until it is shown. Closing a tab leaves its terminal running. On a narrow screen the strip
 * shows only once there is a second tab to switch to.
 */
export function TabStrip({ machines, tabs, active, beside, changed, onClose }: Props) {
  const strip = useRef<HTMLDivElement>(null);
  useEffect(() => {
    strip.current?.querySelector('[aria-selected="true"]')?.scrollIntoView?.({ block: "nearest", inline: "nearest" });
  }, [active, tabs.length]);
  if (tabs.length === 0) return null;
  return (
    <div
      ref={strip}
      role="tablist"
      aria-label="Open terminals"
      className={`${tabs.length < 2 ? "hidden md:flex" : "flex"} h-9 shrink-0 items-stretch overflow-x-auto border-b border-neutral-800 bg-neutral-950 [scrollbar-width:none]`}
    >
      {tabs.map((id) => {
        const found = locate(machines, id);
        const title = found ? `${found.workspace.name} / ${found.name}` : "Terminal";
        const current = id === active;
        const fresh = !current && changed.has(id);
        return (
          <div
            key={id}
            role="presentation"
            onAuxClick={(event) => {
              if (event.button !== 1) return;
              event.preventDefault();
              onClose(id);
            }}
            className={`group relative flex min-w-0 max-w-[45vw] shrink-0 items-center border-r border-neutral-800 md:max-w-60 ${
              current ? "bg-neutral-900 text-neutral-100" : "text-neutral-400 hover:bg-neutral-900/60 hover:text-neutral-200"
            }`}
          >
            {current && <span aria-hidden="true" className="absolute inset-x-0 top-0 h-0.5 bg-sky-400" />}
            {id === beside && <span aria-hidden="true" className="absolute inset-x-0 top-0 h-0.5 bg-sky-400/35" />}
            <Link
              to="/terminals/$terminalId"
              params={{ terminalId: id }}
              role="tab"
              aria-selected={current}
              data-changed={fresh ? "true" : undefined}
              data-beside={id === beside ? "true" : undefined}
              title={found ? `${title} on ${found.machine.name}: ${found.label}` : title}
              className="flex min-w-0 flex-1 items-center gap-2 py-1 pr-1 pl-3 text-xs"
            >
              {found ? <StateGlyph glyph={found.glyph} /> : <span className="size-3.25 shrink-0" />}
              <span className={`truncate ${fresh ? "font-semibold text-neutral-100" : ""}`}>{title}</span>
              {fresh && found && <span className={`shrink-0 font-medium ${glyphText[found.glyph]}`}>{found.label}</span>}
            </Link>
            <button
              type="button"
              aria-label={`Close ${title}`}
              title="Close tab (the terminal keeps running)"
              onClick={() => onClose(id)}
              className={`mr-1 shrink-0 rounded p-1 text-neutral-500 hover:bg-neutral-700 hover:text-neutral-100 ${current ? "" : "md:opacity-0 md:group-hover:opacity-100 md:focus-visible:opacity-100"}`}
            >
              <svg viewBox="0 0 16 16" className="size-3" fill="none" stroke="currentColor" strokeWidth="1.75" aria-hidden="true">
                <path d="M4 4l8 8M12 4l-8 8" strokeLinecap="round" />
              </svg>
            </button>
          </div>
        );
      })}
    </div>
  );
}
