import type { FileChange } from "./api";
import { diffLines, splitPath } from "./changes";
import { CodeLine } from "./CodeLine";
import { paint } from "./paint";
import { fileStatus, marker, rowTone } from "./ChangesPanel";
import { jumpKey } from "./JumpPalette";

/** Diff lines a peek shows before it says how many more there are. */
const PEEK_LINES = 12;

/** What the pointer rests on in a terminal: a web address, or a file its workspace changed. */
export type Peeked = { x: number; y: number } & ({ url: string } | { file: FileChange });

/** The key held to open a link: ⌘ on a Mac, Ctrl elsewhere. */
const holdKey = jumpKey === "⌘K" ? "⌘" : "Ctrl";

/**
 * A card by the pointer while it rests on a link in a terminal: a web address says where it goes,
 * a changed file shows its counts and the start of its diff. Either says how to open it. It takes
 * no pointer, so moving off the link closes it.
 */
export function LinkPeek({ peek }: { peek: Peeked }) {
  const width = 448;
  const left = Math.max(8, Math.min(peek.x + 12, window.innerWidth - width - 8));
  const below = peek.y < window.innerHeight * 0.6;
  const place = below ? { top: peek.y + 18 } : { bottom: window.innerHeight - peek.y + 12 };
  return (
    <div
      role="tooltip"
      style={{ left, maxWidth: `min(${width}px, calc(100vw - 16px))`, ...place }}
      className="pointer-events-none fixed z-40 overflow-hidden rounded-lg border border-neutral-700 bg-neutral-900 text-xs text-neutral-200 shadow-xl shadow-black/50"
    >
      {"url" in peek ? <Address url={peek.url} /> : <File file={peek.file} />}
    </div>
  );
}

function Address({ url }: { url: string }) {
  let host = url;
  let rest = "";
  try {
    const parsed = new URL(url);
    host = parsed.host;
    rest = `${parsed.pathname}${parsed.search}${parsed.hash}`.replace(/^\/$/, "");
  } catch {
    // Not one the browser can read; show it whole.
  }
  return (
    <div className="space-y-1 px-3 py-2">
      <div className="truncate font-mono">
        <span className="text-neutral-100">{host}</span>
        <span className="text-neutral-400">{rest}</span>
      </div>
      <div className="text-neutral-500">{holdKey}-click to open in a new tab</div>
    </div>
  );
}

function File({ file }: { file: FileChange }) {
  const { dir, name } = splitPath(file.path);
  const s = fileStatus[file.status];
  const all = file.patch === null ? [] : diffLines(file.patch);
  const painted = paint(all, file.path);
  const lines = all.map((line, i) => ({ line, pieces: painted[i] })).filter(({ line }) => line.kind !== "note");
  const more = lines.length - PEEK_LINES;
  return (
    <>
      <div className="flex items-center gap-2 border-b border-neutral-800 px-3 py-2">
        <span title={s.title} className={`rounded px-1 font-mono text-[10px] ${s.tone}`}>
          {s.letter}
        </span>
        <span className="min-w-0 flex-1 truncate font-mono">
          <span className="text-neutral-500">{dir}</span>
          <span className="text-neutral-100">{name}</span>
        </span>
        {file.added !== null && (
          <span className="shrink-0 font-mono">
            <span className="text-emerald-400">+{file.added}</span> <span className={file.removed ? "text-red-400" : "text-neutral-600"}>−{file.removed}</span>
          </span>
        )}
        {file.uncommitted && <span className="shrink-0 text-amber-300">uncommitted</span>}
      </div>
      {file.patch !== null ? (
        lines.length > 0 && (
        <div className="overflow-hidden py-1 font-mono text-[11px] leading-[18px]">
          {lines.slice(0, PEEK_LINES).map(({ line, pieces }, i) =>
            line.kind === "hunk" ? (
              <div key={i} className="truncate bg-sky-500/5 px-3 text-sky-300/70">
                {line.text}
              </div>
            ) : (
              <div key={i} className={`flex ${rowTone[line.kind as keyof typeof rowTone]}`}>
                <span className="w-5 shrink-0 select-none text-center opacity-70">{marker[line.kind as keyof typeof marker]}</span>
                <span className="truncate whitespace-pre pr-3">
                  <CodeLine pieces={pieces} kind={line.kind as "added" | "removed" | "same"} />
                </span>
              </div>
            ),
          )}
          {more > 0 && <div className="px-3 pt-1 text-neutral-500">{more} more lines</div>}
        </div>
        )
      ) : (
        <div className="px-3 py-2 text-neutral-500">{file.added === null ? "Binary, or not a plain file." : "Too large to show here."}</div>
      )}
      <div className="border-t border-neutral-800 px-3 py-1.5 text-neutral-500">{holdKey}-click to open in Changes</div>
    </>
  );
}
