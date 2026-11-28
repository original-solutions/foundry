import { useEffect, useState, useSyncExternalStore } from "react";
import { installUpdate, refused } from "./api";
import type { MachineView } from "./fleet";
import { button, primaryButton } from "./Modal";

/** What became of one install call: not sent yet (the serving machine's, while others are out), out, answered, or dropped. */
export type Call = { kind: "queued" } | { kind: "out" } | { kind: "answered"; status: number; json: Record<string, unknown> } | { kind: "dropped" };

/** A machine an install was sent to, with the tag it sent and what became of the call. */
export interface Row {
  id: string;
  name: string;
  tag: string;
  call: Call;
}

/** `X.Y.Z` as three numbers; anything else is not a version. */
function numbers(version: string): number[] | undefined {
  const m = /^(\d+)\.(\d+)\.(\d+)$/.exec(version);
  return m ? m.slice(1).map(Number) : undefined;
}

/** The numbers of tag `vX.Y.Z`. */
const tagNumbers = (tag: string) => (tag.startsWith("v") ? numbers(tag.slice(1)) : undefined);

/** Whether `version` is the version `tag` names, compared as three numbers. */
function runs(version: string | null | undefined, tag: string): boolean {
  const a = version ? numbers(version) : undefined;
  const b = tagNumbers(tag);
  return !!a && !!b && a.every((n, i) => n === b[i]);
}

/** Two versions' numbers compared: below 0 when `a` is the lower, 0 when they are one. */
const compare = (a: number[], b: number[]) => a[0] - b[0] || a[1] - b[1] || a[2] - b[2];

/** Whether `version` is the version `tag` names or a later one, compared as three numbers. */
function atOrPast(version: string | null | undefined, tag: string): boolean {
  const a = version ? numbers(version) : undefined;
  const b = tagNumbers(tag);
  return !!a && !!b && compare(a, b) >= 0;
}

/** Higher tags first; a tag that is not `v` and three numbers last. */
export function newestFirst(a: string, b: string): number {
  const x = tagNumbers(a);
  const y = tagNumbers(b);
  if (!x || !y) return x ? -1 : y ? 1 : a.localeCompare(b);
  return compare(y, x);
}

/** What a sent row says, from its machine's sample and its call's answer, the first match winning. */
export function rowSays(row: Row, machine: MachineView | undefined): { text: string; tone: string } {
  const sample = machine?.sample;
  if (sample?.update_refused === row.tag) return { text: "Refused: it will not run this release", tone: "text-red-300" };
  if (runs(sample?.version, row.tag)) return { text: `Running ${row.tag}`, tone: "text-emerald-300" };
  const { call } = row;
  if (call.kind === "queued") return { text: "Waits for the others: this machine restarts last", tone: "text-neutral-400" };
  if (call.kind === "out") return { text: "Sending…", tone: "text-neutral-400" };
  // Only an answered call is left once it may not have run; the kind check says so to the compiler.
  if (mayHaveRun(call) || call.kind !== "answered") return { text: "Sent, outcome unknown", tone: "text-amber-300" };
  const { status, json } = call;
  const error = refused(status, json).message;
  if (status === 202) return { text: "Restarting", tone: "text-sky-300" };
  if (status === 503 || status === 404) return { text: `Not sent: ${error}`, tone: "text-amber-300" };
  if (status === 409) {
    const ready = typeof json.ready === "string" ? ` · ${json.ready} is ready now` : "";
    return { text: `Already changed: ${error}${ready}`, tone: "text-amber-300" };
  }
  return { text: `Failed: ${error}`, tone: "text-red-300" };
}

/** Whether a call ended with no answer that says what happened: the install may have run. */
export const mayHaveRun = (call: Call) => call.kind === "dropped" || (call.kind === "answered" && call.status === 504);

/**
 * The installs this page sent since it loaded, shared by the checklist and every machine's page,
 * so leaving one for the other keeps what was sent and the hold on sending more.
 */
interface Installs {
  /** The newest install sent to each machine, by id. */
  calls: Record<string, Row>;
  /** The machines the checklist's last Install sent to, in its order, until Done. */
  batch?: string[];
  /** Installs are out: no more may start, so none is cut off by the serving machine's restart. */
  busy: boolean;
  /**
   * The tag the serving machine's own call was for, when it answered 202 (`sure`) or gave no answer
   * (it may have run): it restarts, until its sample runs that tag or a later one, or refused it.
   */
  restartingTo?: { tag: string; sure: boolean };
}

const fresh: Installs = { calls: {}, busy: false };
let installs = fresh;
const listeners = new Set<() => void>();
function change(next: (now: Installs) => Installs) {
  installs = next(installs);
  for (const listener of listeners) listener();
}
const subscribe = (listener: () => void) => {
  listeners.add(listener);
  return () => void listeners.delete(listener);
};
const read = () => installs;

/** Forgets every install sent, as a reload does. */
export function forgetInstalls() {
  change(() => fresh);
}

/**
 * Sends each picked machine its tag, the machine serving the page (`here`) last, so its restart
 * cuts off no other call, and notes what each call became. `batch` marks them as the checklist's.
 */
export async function install(picked: { id: string; name: string; tag: string }[], here: string | undefined, batch: boolean): Promise<void> {
  const rows: Row[] = picked.map((p) => ({ ...p, call: { kind: p.id === here ? "queued" : "out" } }));
  change((s) => ({ ...s, busy: true, batch: batch ? rows.map((r) => r.id) : s.batch, calls: { ...s.calls, ...Object.fromEntries(rows.map((r) => [r.id, r])) } }));
  const settle = (row: Row, call: Call) => change((s) => ({ ...s, calls: { ...s.calls, [row.id]: { ...row, call } } }));
  const send = async (row: Row): Promise<Call> => {
    const call: Call = await installUpdate(row.id, row.tag).then(
      ({ status, json }) => ({ kind: "answered", status, json }),
      () => ({ kind: "dropped" }),
    );
    settle(row, call);
    return call;
  };
  await Promise.all(rows.filter((r) => r.id !== here).map(send));
  const last = rows.find((r) => r.id === here);
  if (last) {
    settle(last, { kind: "out" });
    const call = await send(last);
    if (mayHaveRun(call)) change((s) => ({ ...s, restartingTo: { tag: last.tag, sure: false } }));
    else if (call.kind === "answered" && call.status === 202) change((s) => ({ ...s, restartingTo: { tag: last.tag, sure: true } }));
  }
  change((s) => ({ ...s, busy: false }));
}

/**
 * The installs sent, and `held`: why no install may start now, or nothing. The restart hold lifts
 * for good once the serving machine's sample runs the tag or a later one, or refused it.
 */
export function useInstalls(machines: MachineView[], here: string | undefined) {
  const state = useSyncExternalStore(subscribe, read);
  const served = machines.find((m) => m.id === here)?.sample;
  const to = state.restartingTo;
  const back = to !== undefined && (atOrPast(served?.version, to.tag) || served?.update_refused === to.tag);
  useEffect(() => {
    if (back) change((s) => (s.restartingTo === to ? { ...s, restartingTo: undefined } : s));
  }, [back, to]);
  const restarting = back ? undefined : to;
  const held = restarting?.sure
    ? "The machine serving this page is restarting on the new release. Install again once it is back."
    : restarting
      ? "The machine serving this page may be restarting: its install went out and no answer came. Install again once it is back, or reload the page if it never restarts."
      : state.busy
        ? "The last install is still out. Install again once it has answered."
        : undefined;
  return { calls: state.calls, batch: state.batch, held };
}

/**
 * The checklist that installs the release waiting on the fleet: each listed machine with a release
 * waiting, ticked until unticked, and the tag shown for it. Install sends that tag to each machine
 * ticked, the machine serving the page (`here`) last (`install`). Nothing goes while installs are
 * out or the serving machine restarts (`useInstalls`). Once sent, each machine's row says what
 * became of its call until Done puts the checklist back; the installs keep going after Done, and
 * after the page is left, which keeps the rows and the hold until the page reloads.
 */
export function UpdateChecklist({ machines, here }: { machines: MachineView[]; here: string | undefined }) {
  // Machines unticked by hand; every other waiting machine is ticked, one made ready later too.
  const [unticked, setUnticked] = useState<Set<string>>(new Set());
  const { calls, batch, held } = useInstalls(machines, here);
  const rows = batch?.map((id) => calls[id]);

  const waiting = machines.flatMap((m) => (m.listed && m.sample?.update_ready ? [{ machine: m, tag: m.sample.update_ready }] : []));
  const ticked = (id: string) => !unticked.has(id);

  const done = () => {
    change((s) => ({ ...s, batch: undefined }));
    setUnticked(new Set());
  };

  const installTicked = () =>
    install(
      waiting.filter((w) => ticked(w.machine.id)).map((w) => ({ id: w.machine.id, name: w.machine.name, tag: w.tag })),
      here,
      true,
    );

  return (
    <section aria-labelledby="update-checklist">
      <h3 id="update-checklist" className="text-sm font-semibold text-neutral-100">
        Install the waiting release
      </h3>
      <p className="mt-0.5 text-xs text-neutral-500">Each machine picked installs the release shown for it and restarts. Its terminals keep running.</p>
      {rows ? (
        <ul aria-label="Installs" className="mt-3 flex flex-col gap-1">
          {rows.map((row) => {
            const says = rowSays(row, machines.find((m) => m.id === row.id));
            return (
              <li key={row.id} aria-label={row.name} className="flex flex-wrap items-baseline gap-x-2 rounded-md bg-neutral-800/60 px-3 py-2">
                <span className="font-medium text-neutral-100">{row.name}</span>
                <span className="font-mono text-xs text-neutral-400">{row.tag}</span>
                <span className={`w-full text-xs ${says.tone}`}>{says.text}</span>
              </li>
            );
          })}
        </ul>
      ) : (
        <ul aria-label="Machines with a release waiting" className="mt-3 flex flex-col gap-1">
          {waiting.map(({ machine, tag }) => (
            <li key={machine.id}>
              <label className="flex items-center gap-3 rounded-md px-3 py-2 hover:bg-neutral-800/60">
                <input
                  type="checkbox"
                  aria-label={`${machine.name} ${tag}`}
                  checked={ticked(machine.id)}
                  onChange={(event) => {
                    const next = new Set(unticked);
                    if (event.target.checked) next.delete(machine.id);
                    else next.add(machine.id);
                    setUnticked(next);
                  }}
                />
                <span className="min-w-0 flex-1 truncate font-medium text-neutral-100">{machine.name}</span>
                {machine.id === here && <span className="text-xs text-neutral-500">this machine, last</span>}
                <span className="font-mono text-xs text-neutral-400">{tag}</span>
              </label>
            </li>
          ))}
          {waiting.length === 0 && <li className="px-3 py-2 text-xs text-neutral-500">No machine has a release waiting.</li>}
        </ul>
      )}
      {!rows && held && <p className="mt-3 text-xs text-amber-300">{held}</p>}
      <div className="mt-3 flex justify-end gap-2">
        {rows ? (
          <button type="button" className={button} onClick={done}>
            Done
          </button>
        ) : (
          <button type="button" className={primaryButton} disabled={here === undefined || held !== undefined || !waiting.some((w) => ticked(w.machine.id))} onClick={() => void installTicked()}>
            Install
          </button>
        )}
      </div>
    </section>
  );
}
