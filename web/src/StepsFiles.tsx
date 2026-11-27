import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef } from "react";
import type { FileChange, Mark } from "./api";
import { splitPath } from "./changes";
import { changesQuery, fileStatus } from "./ChangesPanel";
import { plural } from "./labels";
import { touched, type FileTouch, type Ran, type Touch } from "./touched";

/** How Steps shows a session: turn by turn, or by the files and commands it touched. */
export type Lens = "turns" | "files";

/**
 * A session's trail by what it touched instead of when: every file it read or changed, changed
 * ones first, with how often and a strip marking where along the trail; then the commands it ran
 * and its other tools, each once with how often. A file the workspace's changes hold shows its
 * lines and opens its diff. The changes are read on opening and again when the session's state
 * changes. They share the header's query, so once read, the header's changes button reads them
 * again every 15 seconds while the terminal is open; this view adds no timer of its own.
 */
export function StepsFiles({ trail, workspace, label, onFile }: { trail: Mark[]; workspace?: string; label: string; onFile?: (path: string) => void }) {
  const client = useQueryClient();
  const { data } = useQuery({ ...changesQuery(workspace ?? ""), enabled: workspace !== undefined, retry: false });
  const last = useRef(label);
  useEffect(() => {
    if (last.current === label || workspace === undefined) return;
    last.current = label;
    void client.invalidateQueries({ queryKey: changesQuery(workspace).queryKey, exact: true });
  }, [label, workspace, client]);
  const { files, commands, others, turns } = touched(trail);
  const changed = new Map((data?.files ?? []).map((f) => [f.path, f]));
  if (files.length + commands.length + others.length === 0) {
    return <p className="p-4 text-neutral-500">No files or commands yet. Each file Claude reads or changes, and each command it runs, shows here.</p>;
  }
  return (
    <div className="min-h-0 flex-1 space-y-4 overflow-y-auto p-3">
      {files.length > 0 && (
        <Section name="Files" count={files.length}>
          {files.map((file) => (
            <FileRow key={file.path} file={file} turns={turns} change={changed.get(file.path)} known={data !== undefined} onFile={onFile} />
          ))}
        </Section>
      )}
      {commands.length > 0 && (
        <Section name="Commands" count={commands.length}>
          {commands.map((ran) => (
            <RanRow key={ran.text} ran={ran} turns={turns} />
          ))}
        </Section>
      )}
      {others.length > 0 && (
        <Section name="Other tools" count={others.length}>
          {others.map((ran) => (
            <RanRow key={ran.text} ran={ran} turns={turns} />
          ))}
        </Section>
      )}
    </div>
  );
}

function Section({ name, count, children }: { name: string; count: number; children: React.ReactNode }) {
  return (
    <section>
      <h3 className="mb-1.5 flex items-baseline gap-2 px-1 text-[11px] font-medium tracking-wide text-neutral-500 uppercase">
        {name}
        <span className="font-mono tracking-normal text-neutral-600">{count}</span>
      </h3>
      <ul aria-label={name} className="divide-y divide-neutral-800/70 overflow-hidden rounded-lg border border-neutral-800 bg-neutral-900/60">
        {children}
      </ul>
    </section>
  );
}

function FileRow({ file, turns, change, known, onFile }: { file: FileTouch; turns: number[]; change?: FileChange; known: boolean; onFile?: (path: string) => void }) {
  const { dir, name } = splitPath(file.path);
  const status = change ? fileStatus[change.status] : undefined;
  const did = [file.edits > 0 && plural(file.edits, "edit"), file.reads > 0 && plural(file.reads, "read")].filter(Boolean).join(" · ");
  const body = (
    <>
      <span className="flex items-center gap-2">
        <span
          aria-hidden="true"
          title={status?.title}
          className={`grid size-4 shrink-0 place-items-center rounded font-mono text-[10px] leading-none font-semibold ${status ? status.tone : file.edits > 0 ? "bg-amber-400/15 text-amber-300" : "bg-neutral-700/60 text-neutral-300"}`}
        >
          {status ? status.letter : file.edits > 0 ? "±" : "R"}
        </span>
        <span className="min-w-0 flex-1 truncate font-mono text-[11px]" title={file.path}>
          <span className="text-neutral-500">{dir}</span>
          <span className={file.edits > 0 || change ? "text-neutral-100" : "text-neutral-300"}>{name}</span>
        </span>
        {change ? (
          <span className="shrink-0 font-mono text-[11px]">
            <span className="text-emerald-300">+{change.added ?? 0}</span> <span className="text-red-300">−{change.removed ?? 0}</span>
          </span>
        ) : (
          known && file.edits > 0 && <span className="shrink-0 text-[11px] text-neutral-500">no change now</span>
        )}
      </span>
      <Track label={did} touches={file.touches} turns={turns} tone="file" />
    </>
  );
  if (change && onFile) {
    return (
      <li>
        <button
          type="button"
          aria-label={`${file.path}, ${did}, +${change.added ?? 0} −${change.removed ?? 0}: open its diff`}
          onClick={() => onFile(file.path)}
          className="block w-full px-2.5 py-2 text-left hover:bg-neutral-800/60 focus-visible:bg-neutral-800/60 focus-visible:outline-none"
        >
          {body}
        </button>
      </li>
    );
  }
  return <li className="px-2.5 py-2">{body}</li>;
}

function RanRow({ ran, turns }: { ran: Ran; turns: number[] }) {
  return (
    <li className="px-2.5 py-2">
      <span className="flex items-center gap-2">
        <span aria-hidden="true" className={`grid size-4 shrink-0 place-items-center rounded font-mono text-[10px] leading-none font-semibold ${ran.kind === "command" ? "bg-emerald-400/15 text-emerald-300" : "bg-violet-400/15 text-violet-300"}`}>
          {ran.kind === "command" ? "$" : "·"}
        </span>
        <span className="min-w-0 flex-1 truncate font-mono text-[11px] text-neutral-200" title={ran.text}>
          {ran.text}
        </span>
      </span>
      <Track label={ran.count === 1 ? "once" : `${ran.count} times`} touches={ran.touches} turns={turns} tone={ran.kind === "command" ? "command" : "other"} />
    </li>
  );
}

/**
 * A row's second line: what was done, in a column of one width, so every row's strip starts at the
 * same place and a moment of the trail lines up down the whole list.
 */
function Track({ label, ...strip }: { label: string } & Parameters<typeof Strip>[0]) {
  return (
    <span className="mt-1 flex items-center gap-2 pl-6 text-[11px] text-neutral-500">
      <span className="w-24 shrink-0 truncate">{label}</span>
      <Strip {...strip} />
    </span>
  );
}

const ticks = { file: "", command: "bg-emerald-400/80", other: "bg-violet-400/70" };

/** Where along the trail something was touched: a thin track from the first step to the last, a tick for each touch, a gap where each turn starts. */
function Strip({ touches, turns, tone }: { touches: Touch[]; turns: number[]; tone: keyof typeof ticks }) {
  return (
    <span aria-hidden="true" className="relative h-2.5 min-w-12 flex-1 rounded-sm bg-neutral-800/70">
      {turns.map((at) => (
        <span key={`turn-${at}`} style={{ left: `${(at * 100).toFixed(2)}%` }} className="absolute -inset-y-px w-px -translate-x-1/2 bg-neutral-950" />
      ))}
      {touches.map((touch, i) => (
        <span
          key={i}
          style={{ left: `calc(${(touch.at * 100).toFixed(2)}% - ${touch.at}px * 2)` }}
          className={`absolute inset-y-0 w-0.5 rounded-full ${tone === "file" ? (touch.edit ? "bg-amber-300" : "bg-neutral-500") : ticks[tone]}`}
        />
      ))}
    </span>
  );
}
