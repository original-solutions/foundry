import { useState } from "react";
import { installUpdate, refused } from "./api";
import type { MachineView } from "./fleet";
import { button, primaryButton } from "./Modal";

/** What became of one install call: not sent yet (the serving machine's, while others are out), out, answered, or dropped. */
export type Call = { kind: "queued" } | { kind: "out" } | { kind: "answered"; status: number; json: Record<string, unknown> } | { kind: "dropped" };

/** A machine the checklist sent an install to, with the tag it sent, kept until the checklist closes. */
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

/** Higher tags first; a tag that is not `v` and three numbers last. */
export function newestFirst(a: string, b: string): number {
  const x = tagNumbers(a);
  const y = tagNumbers(b);
  if (!x || !y) return x ? -1 : y ? 1 : a.localeCompare(b);
  return y[0] - x[0] || y[1] - x[1] || y[2] - x[2];
}

/** What a sent row says, from its machine's sample and its call's answer, the first match winning. */
export function rowSays(row: Row, machine: MachineView | undefined): { text: string; tone: string } {
  const sample = machine?.sample;
  if (sample?.update_refused === row.tag) return { text: "Refused: it will not run this release", tone: "text-red-300" };
  if (runs(sample?.version, row.tag)) return { text: `Running ${row.tag}`, tone: "text-emerald-300" };
  const { call } = row;
  if (call.kind === "queued") return { text: "Waits for the others: this machine restarts last", tone: "text-neutral-400" };
  if (call.kind === "out") return { text: "Sending…", tone: "text-neutral-400" };
  if (call.kind === "dropped" || call.status === 504) return { text: "Sent, outcome unknown", tone: "text-amber-300" };
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

/**
 * The checklist that installs the release waiting on the fleet: each listed machine with a release
 * waiting, ticked until unticked, and the tag shown for it. Install sends that tag to each machine
 * ticked, the machine serving the page (`here`) last, so its restart cuts off no other call. One
 * batch is out at a time, and none goes while the serving machine restarts: from its 202, or its
 * call ending with no answer, until its sample runs or refused the tag. Once sent, each machine's
 * row says what became of its call until Done puts the checklist back; a batch keeps going after
 * Done. What it sent lives only here, until the page it is on is left or reloaded.
 */
export function UpdateChecklist({ machines, here }: { machines: MachineView[]; here: string | undefined }) {
  // Machines unticked by hand; every other waiting machine is ticked, one made ready later too.
  const [unticked, setUnticked] = useState<Set<string>>(new Set());
  const [rows, setRows] = useState<Row[]>();
  /** A batch is out, from this checklist or one put back since: no other may start, so none is cut off by the serving machine's restart. */
  const [busy, setBusy] = useState(false);
  /**
   * The tag the serving machine's own call was for, when it answered 202 (`sure`) or gave no answer
   * (it may have run): it restarts, until its sample runs or refused that tag.
   */
  const [restartingTo, setRestartingTo] = useState<{ tag: string; sure: boolean }>();

  // Adjusted while rendering: once the serving machine's sample runs or refused the tag, it is back, for good.
  const served = machines.find((m) => m.id === here)?.sample;
  if (restartingTo !== undefined && (runs(served?.version, restartingTo.tag) || served?.update_refused === restartingTo.tag)) setRestartingTo(undefined);

  const waiting = machines.flatMap((m) => (m.listed && m.sample?.update_ready ? [{ machine: m, tag: m.sample.update_ready }] : []));
  const ticked = (id: string) => !unticked.has(id);

  const done = () => {
    setRows(undefined);
    setUnticked(new Set());
  };

  const install = async () => {
    const picked: Row[] = waiting
      .filter((w) => ticked(w.machine.id))
      .map((w) => ({ id: w.machine.id, name: w.machine.name, tag: w.tag, call: { kind: w.machine.id === here ? "queued" : "out" } }));
    setRows(picked);
    setBusy(true);
    // Once Done puts the checklist back its rows are gone, and an answer has no row to land in.
    const settle = (id: string, call: Call) => setRows((now) => now?.map((r) => (r.id === id ? { ...r, call } : r)));
    const send = async (row: Row): Promise<Call> => {
      const call: Call = await installUpdate(row.id, row.tag).then(
        ({ status, json }) => ({ kind: "answered", status, json }),
        () => ({ kind: "dropped" }),
      );
      settle(row.id, call);
      return call;
    };
    await Promise.all(picked.filter((r) => r.id !== here).map(send));
    const last = picked.find((r) => r.id === here);
    if (last) {
      settle(last.id, { kind: "out" });
      const call = await send(last);
      const status = call.kind === "answered" ? call.status : undefined;
      if (call.kind === "dropped" || status === 504) setRestartingTo({ tag: last.tag, sure: false });
      else if (status === 202) setRestartingTo({ tag: last.tag, sure: true });
    }
    setBusy(false);
  };
  const held = restartingTo?.sure
    ? "This machine is restarting on the new release. Install again once it is back."
    : restartingTo
      ? "This machine may be restarting: its install went out and no answer came. Install again once it is back, or reload the page if it never restarts."
      : busy
        ? "The last install is still out. Install again once it has answered."
        : undefined;

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
          <button type="button" className={primaryButton} disabled={here === undefined || held !== undefined || !waiting.some((w) => ticked(w.machine.id))} onClick={() => void install()}>
            Install
          </button>
        )}
      </div>
    </section>
  );
}
