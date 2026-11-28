import type { FactoryRun } from "./fleet";

/** The factory's violet badge, as the mark wears it; a place that cannot hold a link wears it as plain text. */
export const factoryBadge = "shrink-0 rounded border border-violet-500/40 px-1 text-[10px] leading-4 font-medium text-violet-300";

/**
 * Says a workspace is a factory run's, as a link to its issue when the run names one. `compact`
 * shows only the issue (`#1182`), for a place whose name already says it is a factory task; its
 * accessible name still says `factory #1182`.
 */
export function FactoryMark({ run, compact = false }: { run: FactoryRun; compact?: boolean }) {
  const full = run.issue === undefined ? "factory" : `factory #${run.issue}`;
  const text = compact && run.issue !== undefined ? `#${run.issue}` : full;
  const label = text === full ? undefined : full;
  if (!run.url)
    return (
      <span aria-label={label} className={factoryBadge}>
        {text}
      </span>
    );
  return (
    <a href={run.url} target="_blank" rel="noreferrer" aria-label={label} title="Built by the factory: open its issue" className={`${factoryBadge} hover:bg-violet-500/10`}>
      {text}
    </a>
  );
}

/** A factory's roofline: the factory view. */
export function FactoryIcon({ className }: { className?: string }) {
  return (
    <svg aria-hidden="true" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinejoin="round" className={className}>
      <path d="M2 14V7l4 2.5V7l4 2.5V3h4v11z" />
    </svg>
  );
}
