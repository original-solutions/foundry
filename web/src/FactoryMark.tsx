import type { FactoryRun } from "./fleet";

/**
 * Says a workspace is a factory run's, as a link to its issue when the run names one. `compact`
 * shows only the issue (`#1182`), for a place whose name already says it is a factory task; its
 * accessible name still says `factory #1182`.
 */
export function FactoryMark({ run, compact = false }: { run: FactoryRun; compact?: boolean }) {
  const full = run.issue === undefined ? "factory" : `factory #${run.issue}`;
  const text = compact && run.issue !== undefined ? `#${run.issue}` : full;
  const label = text === full ? undefined : full;
  const shape = "shrink-0 rounded border border-violet-500/40 px-1 text-[10px] leading-4 font-medium text-violet-300";
  if (!run.url)
    return (
      <span aria-label={label} className={shape}>
        {text}
      </span>
    );
  return (
    <a href={run.url} target="_blank" rel="noreferrer" aria-label={label} title="Built by the factory: open its issue" className={`${shape} hover:bg-violet-500/10`}>
      {text}
    </a>
  );
}
