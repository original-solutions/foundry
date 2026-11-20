import type { Tone } from "./labels";

const dot: Record<Tone, string> = {
  attention: "bg-amber-400 shadow-[0_0_6px_1px] shadow-amber-400/70",
  bad: "bg-red-500",
  busy: "bg-sky-400",
  ready: "bg-emerald-400",
  quiet: "bg-neutral-500",
  away: "border border-neutral-500",
};

const pill: Record<Tone, string> = {
  attention: "bg-amber-400/15 text-amber-300",
  bad: "bg-red-500/15 text-red-300",
  busy: "bg-sky-400/15 text-sky-300",
  ready: "bg-emerald-400/10 text-emerald-300",
  quiet: "bg-neutral-700/60 text-neutral-300",
  away: "bg-neutral-800 text-neutral-400",
};

/**
 * A coloured dot for a state. Keyed by `label`, so it plays its one pop when the state changes
 * and never otherwise.
 */
export function StateDot({ tone, label }: { tone: Tone; label: string }) {
  return <span key={label} aria-hidden="true" className={`inline-block size-2 shrink-0 rounded-full motion-safe:animate-state-in ${dot[tone]}`} />;
}

/** The state as a pill: its dot and its label. */
export function StatePill({ tone, label }: { tone: Tone; label: string }) {
  return (
    <span className={`inline-flex shrink-0 items-center gap-1.5 rounded-full px-2.5 py-0.5 text-xs font-medium ${pill[tone]}`}>
      <StateDot tone={tone} label={label} />
      <span>{label}</span>
    </span>
  );
}
