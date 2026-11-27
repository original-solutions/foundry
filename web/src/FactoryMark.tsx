import type { FactoryRun } from "./fleet";

/** Says a workspace is a factory run's, as a link to its issue when the run names one. */
export function FactoryMark({ run }: { run: FactoryRun }) {
  const text = run.issue === undefined ? "factory" : `factory #${run.issue}`;
  const shape = "shrink-0 rounded border border-violet-500/40 px-1 text-[10px] leading-4 font-medium text-violet-300";
  if (!run.url) return <span className={shape}>{text}</span>;
  return (
    <a href={run.url} target="_blank" rel="noreferrer" title="Built by the factory: open its issue" className={`${shape} hover:bg-violet-500/10`}>
      {text}
    </a>
  );
}
