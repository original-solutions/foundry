import { SparkIcon } from "./FirstPrompt";
import { button, primaryButton } from "./Modal";
import type { Room } from "./room";

/**
 * The machines view's head: the machine a task Foundry chooses the machine for would go to now,
 * and how many have room, or, when none has, that such a task would wait and why each machine
 * was passed over. Its button opens the task box set to let Foundry choose.
 */
export function NextTaskHead({
  pick,
  count,
  roomy,
  reasons,
  onCreate,
}: {
  /** The name of the machine Foundry would pick, if any. */
  pick?: string;
  /** How many machines the fleet has, and how many of them have room. */
  count: number;
  roomy: number;
  /** Why each machine was passed over, as the daemon words it. */
  reasons: string[];
  onCreate?: () => void;
}) {
  return (
    <section
      aria-label="Next task"
      className={`flex min-w-0 basis-full flex-wrap items-start gap-3 rounded-xl border px-4 py-3 md:basis-0 md:flex-1 ${pick ? "border-emerald-400/25 bg-emerald-400/[0.04]" : "border-amber-400/30 bg-amber-400/[0.05]"}`}
    >
      <span
        aria-hidden="true"
        className={`mt-0.5 grid size-8 shrink-0 place-items-center rounded-lg ${pick ? "bg-emerald-400/10 text-emerald-300" : "bg-amber-400/10 text-amber-300"}`}
      >
        {pick ? <SparkIcon className="size-4" /> : <span className="text-sm font-bold">!</span>}
      </span>
      <div className="min-w-0 flex-1 basis-48">
        <p className="text-[11px] font-semibold tracking-[0.18em] text-neutral-500 uppercase">If Foundry chooses</p>
        {pick ? (
          <p className="text-[15px] font-semibold text-neutral-100">
            <span className="text-emerald-300">{pick}</span> takes the next task
          </p>
        ) : (
          <p className="text-[15px] font-semibold text-amber-200">No machine has room</p>
        )}
        <p className="mt-0.5 text-xs text-neutral-400">
          {pick
            ? `${roomy} of ${count} machines ${roomy === 1 ? "has" : "have"} room: online and under ${roomy === 1 ? "its" : "their"} limits. The least loaded wins.`
            : "A task started now waits until one does."}
        </p>
        {!pick && reasons.length > 0 && (
          <ul aria-label="Why each machine was passed over" className="mt-1.5 flex flex-col gap-0.5 font-mono text-[11px] text-amber-300/80">
            {reasons.map((reason) => (
              <li key={reason} className="truncate" title={reason}>
                {reason}
              </li>
            ))}
          </ul>
        )}
      </div>
      {onCreate && (
        <button type="button" onClick={onCreate} aria-label="New task, Foundry chooses the machine" className={`${primaryButton} shrink-0 self-center px-3 py-1 max-md:ml-11`}>
          New task
        </button>
      )}
    </section>
  );
}

const chip = "shrink-0 rounded-full px-2 py-px text-[11px] font-medium";

/**
 * A machine card's line on the next task: Foundry's pick, room, or full and which limits, with a
 * button that opens the task box set to this machine. An offline machine, or one the page cannot
 * say of, shows neither.
 */
export function RoomLine({ name, room, several, onCreate }: { name: string; room: Room; several: boolean; onCreate?: () => void }) {
  if (room.kind === "offline" || room.kind === "unknown") return null;
  return (
    <div className="flex items-center justify-between gap-2 px-4 pt-2">
      <p className="flex min-w-0 items-center gap-1.5 text-xs">
        {room.kind === "full" ? (
          <>
            <span className={`${chip} bg-amber-400/10 text-amber-300`}>Full</span>
            <span className="truncate text-amber-300/80" title={room.over.join(", ")}>
              {room.over.join(", ")}
            </span>
          </>
        ) : room.kind === "pick" && several ? (
          <span className={`${chip} flex items-center gap-1 bg-emerald-400/10 text-emerald-300`}>
            <SparkIcon className="size-3" />
            Foundry's pick
          </span>
        ) : (
          <span className={`${chip} bg-neutral-800 text-neutral-300`}>Has room</span>
        )}
      </p>
      {onCreate && (
        <button type="button" onClick={onCreate} aria-label={`New task on ${name}`} className={`${button} shrink-0`}>
          New task here
        </button>
      )}
    </div>
  );
}
