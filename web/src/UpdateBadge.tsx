import { useRef, useState } from "react";
import { installUpdate, refused } from "./api";
import type { MachineView } from "./fleet";
import { plural } from "./labels";
import { button, Modal, primaryButton } from "./Modal";

/** What became of one install call: not sent yet (the serving machine's, while others are out), out, answered, or dropped. */
type Call = { kind: "queued" } | { kind: "out" } | { kind: "answered"; status: number; json: Record<string, unknown> } | { kind: "dropped" };

/** A machine the checklist sent an install to, with the tag it sent, kept until the checklist closes. */
interface Row {
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
function newestFirst(a: string, b: string): number {
  const x = tagNumbers(a);
  const y = tagNumbers(b);
  if (!x || !y) return x ? -1 : y ? 1 : a.localeCompare(b);
  return y[0] - x[0] || y[1] - x[1] || y[2] - x[2];
}

/** What a sent row says, from its machine's sample and its call's answer, the first match winning. */
function rowSays(row: Row, machine: MachineView | undefined): { text: string; tone: string } {
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
 * The release waiting on the fleet, in the page's top bar: each waiting tag and how many listed
 * machines wait on it. It opens a checklist that installs the tag shown for each machine ticked,
 * the machine serving the page (`here`) last, so its restart cuts off no other call. What the
 * checklist sent lives only here, until it closes or the page reloads. Nothing shows while no
 * listed machine has a release waiting and the checklist is closed.
 */
export function UpdateBadge({ machines, here }: { machines: MachineView[]; here: string | undefined }) {
  const [open, setOpen] = useState(false);
  const [ticked, setTicked] = useState<Set<string>>(new Set());
  const [rows, setRows] = useState<Row[]>();
  /** The batch the open checklist sent: an answer to an earlier one, closed since, is dropped, and its serving machine's call never goes. */
  const batch = useRef(0);
  /** Every install call still out, from any batch: the serving machine's call waits for them all. */
  const out = useRef(new Set<Promise<void>>());

  const waiting = machines.flatMap((m) => (m.listed && m.sample?.update_ready ? [{ machine: m, tag: m.sample.update_ready }] : []));
  if (waiting.length === 0 && !open) return null;

  const counts = new Map<string, number>();
  for (const w of waiting) counts.set(w.tag, (counts.get(w.tag) ?? 0) + 1);
  const groups = [...counts].sort(([a], [b]) => newestFirst(a, b));
  const summary = groups.map(([tag, n], i) => `${tag} on ${i === 0 ? plural(n, "machine") : n}`).join(", ");

  const onOpenChange = (next: boolean) => {
    batch.current += 1;
    setOpen(next);
    setRows(undefined);
    setTicked(new Set(next ? waiting.map((w) => w.machine.id) : []));
  };

  const install = async () => {
    const mine = batch.current;
    const picked: Row[] = waiting
      .filter((w) => ticked.has(w.machine.id))
      .map((w) => ({ id: w.machine.id, name: w.machine.name, tag: w.tag, call: { kind: w.machine.id === here ? "queued" : "out" } }));
    setRows(picked);
    const settle = (id: string, call: Call) => {
      if (batch.current === mine) setRows((now) => now?.map((r) => (r.id === id ? { ...r, call } : r)));
    };
    const send = (row: Row) => {
      const call = installUpdate(row.id, row.tag)
        .then(
          ({ status, json }) => settle(row.id, { kind: "answered", status, json }),
          () => settle(row.id, { kind: "dropped" }),
        )
        .finally(() => out.current.delete(call));
      out.current.add(call);
      return call;
    };
    for (const row of picked) if (row.id !== here) void send(row);
    while (out.current.size > 0) await Promise.all(out.current);
    const last = picked.find((r) => r.id === here);
    if (last && batch.current === mine) {
      settle(last.id, { kind: "out" });
      await send(last);
    }
  };

  const trigger = (
    <button
      type="button"
      className="flex max-w-full items-center gap-2 rounded-full border border-sky-400/40 bg-sky-400/10 px-3 py-1 text-xs text-sky-200 hover:bg-sky-400/20"
    >
      <span aria-hidden="true" className="size-1.5 shrink-0 rounded-full bg-sky-300" />
      <span className="min-w-0 truncate">{waiting.length > 0 ? `Release waiting: ${summary}` : "No release waiting"}</span>
    </button>
  );

  return (
    <div className="flex shrink-0 justify-end px-2 pt-2">
      <Modal
        open={open}
        onOpenChange={onOpenChange}
        trigger={trigger}
        title="Install the waiting release"
        description="Each machine picked installs the release shown for it and restarts. Its terminals keep running."
        wide
      >
        {rows ? (
          <ul aria-label="Installs" className="mt-4 flex flex-col gap-1">
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
          <ul aria-label="Machines with a release waiting" className="mt-4 flex flex-col gap-1">
            {waiting.map(({ machine, tag }) => (
              <li key={machine.id}>
                <label className="flex items-center gap-3 rounded-md px-3 py-2 hover:bg-neutral-800/60">
                  <input
                    type="checkbox"
                    aria-label={`${machine.name} ${tag}`}
                    checked={ticked.has(machine.id)}
                    onChange={(event) => {
                      const next = new Set(ticked);
                      if (event.target.checked) next.add(machine.id);
                      else next.delete(machine.id);
                      setTicked(next);
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
        <div className="mt-4 flex justify-end gap-2">
          <button type="button" className={button} onClick={() => onOpenChange(false)}>
            Close
          </button>
          {!rows && (
            <button type="button" className={primaryButton} disabled={here === undefined || !waiting.some((w) => ticked.has(w.machine.id))} onClick={() => void install()}>
              Install
            </button>
          )}
        </div>
      </Modal>
    </div>
  );
}
