import { useId, useState } from "react";
import type { FactorySettings } from "./api";
import { settingsLine, type Tally } from "./factory";
import { FleetSettings } from "./FactorySettings";
import { plural } from "./labels";

/** The line's stations in the order an issue moves along them, each with the colour it shows in when something stands there. */
export const STATIONS: { key: keyof Tally; label: string; text: string; dot: string }[] = [
  { key: "ready", label: "Ready", text: "text-neutral-100", dot: "bg-neutral-300" },
  { key: "building", label: "Building", text: "text-sky-300", dot: "bg-sky-400" },
  { key: "held", label: "Held up", text: "text-amber-300", dot: "bg-amber-400" },
  { key: "finished", label: "Finished", text: "text-emerald-300", dot: "bg-emerald-400" },
];

/**
 * The factory as a line an issue moves along, from ready to finished, with how many stand at each
 * station: big at the head of the view for every project that is on, small beside one project's
 * name. A station with nothing at it is greyed, and the building one pulses while a run builds.
 */
export function Line({ tally, label, small = false }: { tally: Tally; label: string; small?: boolean }) {
  if (small) {
    return (
      <ol aria-label={label} className="flex flex-wrap items-center gap-x-1.5 text-xs text-neutral-500">
        {STATIONS.map((s, i) => (
          <li key={s.key} className={i > 0 ? "before:mr-1.5 before:text-neutral-700 before:content-['›']" : ""}>
            <span className={`font-medium tabular-nums ${tally[s.key] > 0 ? s.text : "text-neutral-600"}`}>{tally[s.key]}</span> <span>{s.label}</span>
          </li>
        ))}
      </ol>
    );
  }
  return (
    <div className="relative">
      {/* The rail the stations sit on, from the first dot to the last. */}
      <span aria-hidden className="absolute top-[0.3125rem] right-[12.5%] left-[12.5%] h-px bg-gradient-to-r from-neutral-600 via-sky-400/40 to-emerald-400/40" />
      <ol aria-label={label} className="relative grid grid-cols-4">
        {STATIONS.map((s) => {
          const n = tally[s.key];
          return (
            <li key={s.key} className="relative flex flex-col items-center text-center">
              <span aria-hidden className={`size-2.5 rounded-full ring-4 ring-neutral-950 ${n > 0 ? s.dot : "bg-neutral-700"} ${s.key === "building" && n > 0 ? "animate-ripple" : ""}`} />
              <span className={`mt-2 text-2xl font-semibold tabular-nums sm:text-3xl ${n > 0 ? s.text : "text-neutral-600"}`}>{n}</span>{" "}
              <span className="text-[0.6875rem] font-medium uppercase tracking-wide text-neutral-500">{s.label}</span>
            </li>
          );
        })}
      </ol>
    </div>
  );
}

/**
 * The fleet's factory settings folded to one line that says them all, which opens to the form.
 * Settings change seldom; what the factory is doing is what the view leads with.
 */
export function SettingsFold({ settings }: { settings: FactorySettings }) {
  const [open, setOpen] = useState(false);
  const id = useId();
  return (
    <div className="rounded-lg border border-neutral-800">
      <button
        type="button"
        aria-expanded={open}
        aria-controls={id}
        aria-labelledby={`${id}-name`}
        aria-describedby={`${id}-line`}
        onClick={() => setOpen((o) => !o)}
        className="flex w-full flex-wrap items-baseline gap-x-3 gap-y-0.5 rounded-lg px-3 py-2 text-left hover:bg-neutral-900"
      >
        <span id={`${id}-name`} className="text-xs font-medium text-neutral-200">
          <span aria-hidden className={`mr-1.5 inline-block text-neutral-500 transition-transform ${open ? "rotate-90" : ""}`}>
            ›
          </span>
          Settings for every project
        </span>
        <span id={`${id}-line`} className="min-w-0 font-mono text-xs text-neutral-500">{settingsLine(settings)}</span>
      </button>
      <div id={id} hidden={!open} className="border-t border-neutral-800 [&>form]:rounded-none [&>form]:border-0">
        {open && <FleetSettings settings={settings} />}
      </div>
    </div>
  );
}

/** What the head says under the line: how many projects it counts, and whether claims are paused for all of them. */
export function LineNote({ on, total, paused }: { on: number; total: number; paused: boolean }) {
  return (
    <p className="mt-3 flex flex-wrap items-center justify-center gap-x-3 gap-y-1 text-xs text-neutral-500">
      <span>{on === total ? `across ${plural(on, "project")}` : `across ${on} of ${plural(total, "project")}`}</span>
      {paused && <span className="rounded-full border border-amber-400/40 bg-amber-400/10 px-2 py-0.5 text-amber-200">Claims are paused for every project</span>}
    </p>
  );
}

/** What the head says under an empty line when no project has its factory on: what the factory does, what it needs, and where to turn it on. */
export function Intro({ paused }: { paused: boolean }) {
  return (
    <div className="mt-4 text-center">
      <p className="text-sm text-neutral-200">Nothing is on the line yet.</p>
      <p className="mx-auto mt-1 max-w-xl text-xs leading-relaxed text-neutral-400">
        The factory builds the GitHub issues labelled <code className="font-mono text-neutral-300">factory:ready</code> into pull requests, on your machines. Turn it on for a project below. The project
        needs a <code className="font-mono text-neutral-300">https://github.com/…</code> repo with a <code className="font-mono text-neutral-300">.dw/config.yml</code>, and the machine that runs it a
        token for its login.
      </p>
      {paused && <p className="mt-2 text-xs text-amber-200">Claims are paused for every project</p>}
    </div>
  );
}
