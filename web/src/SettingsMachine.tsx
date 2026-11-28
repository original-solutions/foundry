import { useMutation } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useId, useState, type ReactNode } from "react";
import type { MachineView } from "./fleet";
import { MachineKept } from "./MachineKept";
import { limitsText, removeMachine, renameMachine, sendLimits } from "./machineApi";
import { button, dangerButton, field, Modal, primaryButton } from "./Modal";
import { diff, type Change, type Field, type Refusal } from "./reviewDiff";
import { ReviewBar, useLeaveGuard } from "./ReviewBar";
import { Page, Row, type PageProps } from "./SettingsPage";
import { install, rowSays, useInstalls } from "./UpdateChecklist";

/**
 * Settings → one machine's page, at `/settings/machines/$machine`, with the same sections in the
 * same order for every machine: its waiting update, its name and limits in the shared settings,
 * what it keeps in its own secret store, and Remove from the fleet. An id the fleet does not hold
 * says so, with the way back to Settings → Machines.
 */
export function SettingsMachine({ state, machines, here, machineId }: PageProps & { machineId: string }) {
  const machine = machines.find((m) => m.id === machineId);
  if (!machine)
    return (
      <Page title={machineId}>
        <p className="text-sm text-neutral-400">No machine {machineId} in this fleet</p>
        <Link to="/settings/$category" params={{ category: "machines" }} className="mt-2 inline-block text-sm text-sky-300 hover:underline">
          Settings → Machines
        </Link>
      </Page>
    );
  return (
    <Page title={machine.name}>
      {/* Keyed, so a page opened for another machine starts with none of this one's state. */}
      <div key={machine.id} className="flex flex-col gap-8">
        <Update machine={machine} machines={machines} here={here} />
        <Shared machine={machine} />
        <MachineKept machine={machine} held={state.held[machine.id]} protocols={state.protocols} here={here} />
        {machine.listed && here !== undefined && machine.id !== here && <Remove machine={machine} />}
      </div>
    </Page>
  );
}

function Section({ id, title, children }: { id: string; title: string; children: ReactNode }) {
  return (
    <section aria-labelledby={id}>
      <h3 id={id} className="text-sm font-semibold text-neutral-100">
        {title}
      </h3>
      {children}
    </section>
  );
}

/**
 * The release ready on the machine, while one is: "{tag} is ready" and Install, which sends that
 * tag and then says what became of the call as the checklist does, whichever of the two sent it.
 * On the machine serving the page it says first that the machine restarts. Install is not offered
 * while any install is out or the serving machine restarts (`useInstalls`), nor while this
 * machine's own call for that tag is out or answered 202. It waits, as the checklist does, until
 * the page knows which machine serves it, since that decides the restart warning and hold.
 */
function Update({ machine, machines, here }: { machine: MachineView; machines: MachineView[]; here: string | undefined }) {
  const id = useId();
  const { calls, held } = useInstalls(machines, here);
  const tag = machine.sample?.update_ready ?? undefined;
  if (!tag) return null;

  const sent = calls[machine.id];
  const call = sent?.call;
  const going = sent?.tag === tag && (call?.kind === "queued" || call?.kind === "out" || (call?.kind === "answered" && call.status === 202));
  const says = sent?.tag === tag && rowSays(sent, machine);

  return (
    <Section id={id} title="Update">
      <p className="mt-1 text-sm text-neutral-200">{tag} is ready</p>
      {machine.id === here && (
        <p className="mt-1 text-xs text-amber-300">This machine serves this page: it restarts to install the release, and the page reconnects once it is back. Its terminals keep running.</p>
      )}
      {says && <p className={`mt-1 text-xs ${says.tone}`}>{says.text}</p>}
      {held && !going && <p className="mt-1 text-xs text-amber-300">{held}</p>}
      {!held && !going && (
        <button type="button" className={`${primaryButton} mt-2`} disabled={here === undefined} onClick={() => void install([{ id: machine.id, name: machine.name, tag }], here, false)}>
          Install
        </button>
      )}
    </Section>
  );
}

/** The shared fields as text, so a half-typed number is kept as typed; a machine with no limits has them blank. */
function texts(machine: MachineView) {
  const l = machine.limits;
  return {
    name: machine.name,
    max_sessions: l ? String(l.max_sessions) : "",
    max_load_per_core: l ? String(l.max_load_per_core) : "",
    min_free_disk_gb: l ? String(l.min_free_disk / 1e9) : "",
  };
}
type Texts = ReturnType<typeof texts>;
type Key = keyof Texts;
const limitKeys = ["max_sessions", "max_load_per_core", "min_free_disk_gb"] as const;

const blankIsNone = (v: string) => (v.trim() === "" ? "none" : v.trim());
const defs: (Field<Texts> & { help: string; attrs?: { min: number | string; step: number | string } })[] = [
  { key: "name", label: "Name", help: "How every machine shows it.", show: (v) => v.trim() },
  { key: "max_sessions", label: "Max sessions", help: "Placement skips this machine while it has this many sessions or more.", show: blankIsNone, attrs: { min: 1, step: 1 } },
  { key: "max_load_per_core", label: "Max load per core", help: "Placement skips it at this load per core or more.", show: blankIsNone, attrs: { min: 0, step: "any" } },
  { key: "min_free_disk_gb", label: "Min free disk (GB)", help: "Placement skips it with less free disk than this.", show: blankIsNone, attrs: { min: 0, step: "any" } },
];

/**
 * The fields of `draft` that differ from `saved`, as the review bar lists them. The name is compared
 * as text: `diff` reads text of digits as a number, so a machine named "1" renamed "01" would be no change.
 */
function changed(saved: Texts, draft: Texts): Change[] {
  const renamed = saved.name === draft.name ? [] : [{ key: "name", label: "Name", before: saved.name.trim(), after: draft.name.trim() }];
  return [...renamed, ...diff(saved, draft, defs.filter((def) => def.key !== "name"))];
}

const asNumber = (text: string) => (text.trim() === "" ? null : Number(text));

/** The first limit outside the range the daemon takes, with the daemon's own sentence for it, or nothing. */
function checkLimits(t: Texts): Refusal | undefined {
  const sessions = asNumber(t.max_sessions);
  if (sessions === null || !Number.isInteger(sessions) || sessions < 1 || sessions > 4294967295)
    return { key: "max_sessions", message: "the maximum sessions is a whole number from 1 to 4294967295" };
  const load = asNumber(t.max_load_per_core);
  if (load === null || !Number.isFinite(load) || load <= 0) return { key: "max_load_per_core", message: "the maximum load per core is a number above 0" };
  const disk = asNumber(t.min_free_disk_gb);
  if (disk === null || !Number.isFinite(disk) || disk < 0) return { key: "min_free_disk_gb", message: "the minimum free disk is a number of gigabytes, 0 or more" };
  return undefined;
}

const without = <T extends object>(from: T, keys: readonly (keyof T)[]): T => {
  const out = { ...from };
  for (const key of keys) delete out[key];
  return out;
};

/**
 * "In the shared settings": the machine's name and placement limits. Edits collect in the review
 * bar, each as old → new; Clear limits is one change, the limits → none. Save sends one rename and
 * one limits request (or a clear) for what changed, each built from the newest settings plus the
 * edits; one that succeeds leaves the bar and shows what it sent until the settings carry a change,
 * and one refused stays with the daemon's reason. Untouched fields follow the settings; when they
 * move a field being edited, the row says so and Save asks for a fresh look first. Editable while
 * the machine is offline: these are fleet settings, applied by the coordinator.
 */
function Shared({ machine }: { machine: MachineView }) {
  const id = useId();
  // `out`: the fields a save sent and has not heard back on.
  const [draft, setDraft] = useState<{ edits: Partial<Texts>; from: Partial<Texts>; out?: Key[] }>({ edits: {}, from: {} });
  // What a save sent, by field, with the saved value then: shown until the saved value moves.
  const [sentOut, setSentOut] = useState<Partial<Record<Key, { value: string; was: string }>>>({});
  const [refusals, setRefusals] = useState<Refusal[]>([]);
  const { edits, from } = draft;

  const saved = texts(machine);
  const landed = (Object.keys(sentOut) as Key[]).filter((key) => sentOut[key]!.was !== saved[key]);
  if (landed.length > 0) setSentOut((s) => without(s, landed));
  const now: Texts = { ...saved };
  for (const key of Object.keys(sentOut) as Key[]) if (!landed.includes(key)) now[key] = sentOut[key]!.value;

  const values: Texts = { ...now, ...edits };
  const blank = (t: Texts) => limitKeys.every((k) => t[k].trim() === "");
  const clearing = blank(values) && !blank(now);
  const fieldChanges = changed(now, { ...values, name: values.name.trim() });
  const changes = clearing
    ? [...fieldChanges.filter((c) => c.key === "name"), { key: "limits", label: "Limits", before: machine.limits ? limitsText(machine.limits) : "", after: "none" }]
    : fieldChanges;
  const moved = new Set(changed({ ...now, ...from }, now).map((c) => c.key));
  const staleFields = fieldChanges.map((c) => c.key).filter((key) => moved.has(key));
  const stale = clearing ? [...staleFields.filter((k) => k === "name"), ...(limitKeys.some((k) => moved.has(k)) ? ["limits"] : [])] : staleFields;
  useLeaveGuard(changes.length > 0);

  const listed = machine.listed;
  const set = (patch: Partial<Texts>) => {
    const keys = Object.keys(patch) as Key[];
    setRefusals((r) => r.filter((x) => x.key === undefined || !(keys as string[]).includes(x.key) && !(x.key === "limits" && keys.some((k) => k !== "name"))));
    setDraft((d) => {
      let edits = { ...d.edits };
      let from = { ...d.from };
      for (const key of keys) {
        // Typed back to exactly what the settings hold, it is untouched again and follows them. A
        // field whose save is out is kept even then: what the save makes the settings decides.
        if (patch[key] === now[key] && !d.out?.includes(key)) {
          edits = without(edits, [key]);
          from = without(from, [key]);
        } else {
          edits[key] = patch[key];
          from[key] = now[key];
        }
      }
      return { ...d, edits, from };
    });
  };

  const discard = () => {
    setDraft({ edits: {}, from: {} });
    setRefusals([]);
  };

  /** Takes what the settings hold now as seen for every stale field, so each is marked again only if it moves again. */
  const look = () => setDraft((d) => ({ ...d, from: { ...d.from, ...Object.fromEntries([...moved].map((key) => [key, now[key as Key]])) } }));

  const save = async (): Promise<Refusal[]> => {
    const sending: Partial<Texts> = { ...edits };
    const name = values.name.trim();
    const renaming = changes.some((c) => c.key === "name");
    const limiting = changes.some((c) => c.key !== "name");
    const limitKey = changes.find((c) => c.key !== "name")?.key;
    const sends: Promise<{ keys: readonly Key[]; error?: Refusal }>[] = [];
    const answer = (keys: readonly Key[], key: string | undefined, sent: Promise<void>) =>
      sent.then(
        () => ({ keys }),
        (e: unknown) => ({ keys, error: { key, message: e instanceof Error ? e.message : "The change was not saved." } }),
      );
    if (renaming) sends.push(answer(["name"], "name", renameMachine({ id: machine.id, name })));
    const wrong = limiting && !clearing ? checkLimits(values) : undefined;
    if (wrong) sends.push(Promise.resolve({ keys: [], error: wrong }));
    else if (limiting) {
      const action = clearing
        ? ("clear" as const)
        : { fields: { max_sessions: asNumber(values.max_sessions), max_load_per_core: asNumber(values.max_load_per_core), min_free_disk_gb: asNumber(values.min_free_disk_gb) } };
      sends.push(answer(limitKeys, limitKey, sendLimits({ id: machine.id, action })));
    }
    const out: Key[] = [...(renaming ? (["name"] as const) : []), ...(limiting && !wrong ? limitKeys : [])];
    setDraft((d) => ({ ...d, out }));
    const results = await Promise.all(sends);
    const done = results.filter((r) => !r.error).flatMap((r) => r.keys);
    const sentValues: Texts = { ...values, name };
    setSentOut((s) => ({ ...s, ...Object.fromEntries(done.map((key) => [key, { value: sentValues[key], was: saved[key] }])) }));
    // Typing since the send is kept.
    setDraft((d) => {
      const kept = done.filter((key) => d.edits[key] !== sending[key]);
      // A refused field typed back to what the settings held then is untouched again.
      const back = out.filter((key) => !done.includes(key) && d.edits[key] === d.from[key]);
      const gone = [...done.filter((key) => !kept.includes(key)), ...back];
      const from = without(d.from, gone);
      // What this page just sent is the baseline for what was typed since.
      for (const key of kept) from[key] = sentValues[key];
      return { edits: without(d.edits, gone), from };
    });
    const found = results.flatMap((r) => (r.error ? [r.error] : []));
    setRefusals(found);
    return found;
  };

  return (
    <Section id={id} title="In the shared settings">
      {!listed && <p className="mt-1 text-xs text-neutral-500">The fleet's machine list does not hold this machine yet, so its name and limits cannot be set.</p>}
      <div className="mt-2">
        {defs.map((def) => {
          const key = def.key;
          const control = `${id}-${key}`;
          // A refused Clear limits is about the limits as a whole, so every limit row shows it.
          const refusal = refusals.find((r) => r.key === key || (r.key === "limits" && key !== "name"));
          const isStale = stale.includes(key) || (clearing && key !== "name" && stale.includes("limits"));
          return (
            <Row
              key={key}
              label={def.label}
              help={def.help}
              htmlFor={control}
              changed={changes.some((c) => c.key === key || (c.key === "limits" && key !== "name"))}
              stale={isStale}
              note={
                (refusal || isStale) && (
                  <p className={`mt-1 text-xs ${refusal ? "text-red-400" : "text-amber-300"}`}>
                    {refusal ? refusal.message : `This changed elsewhere, to ${blankIsNone(now[key])}, since you edited it.`}
                  </p>
                )
              }
            >
              {def.attrs ? (
                <input
                  id={control}
                  type="number"
                  inputMode="decimal"
                  {...def.attrs}
                  disabled={!listed}
                  value={values[key]}
                  onChange={(e) => set({ [key]: e.target.value })}
                  className={field}
                />
              ) : (
                <input id={control} autoComplete="off" spellCheck={false} disabled={!listed} value={values[key]} onChange={(e) => set({ [key]: e.target.value })} className={field} />
              )}
            </Row>
          );
        })}
      </div>
      <div className="mt-3 flex justify-end">
        <button type="button" className={button} disabled={!listed || blank(values)} onClick={() => set({ max_sessions: "", max_load_per_core: "", min_free_disk_gb: "" })}>
          Clear limits
        </button>
      </div>
      <ReviewBar changes={changes} onSave={save} onDiscard={discard} stale={stale} onLook={look} />
    </Section>
  );
}

/**
 * Remove from the fleet, which asks once more. Nothing is sent until the person confirms; the
 * dialog closes once the daemon takes it, and a refusal shows in it.
 */
function Remove({ machine }: { machine: MachineView }) {
  const id = useId();
  const [open, setOpen] = useState(false);
  const remove = useMutation({ mutationFn: removeMachine, onSuccess: () => setOpen(false) });
  const change = (next: boolean) => {
    remove.reset();
    setOpen(next);
  };
  return (
    <Section id={id} title="Remove from the fleet">
      <div className="mt-2">
        <Modal
          open={open}
          onOpenChange={change}
          trigger="Remove from fleet"
          title={`Remove ${machine.name}`}
          description={`${machine.name} will be cut off from every machine and cannot join again under its current identity. Its workspaces stay on its disk.`}
        >
          <div className="mt-3 flex flex-col gap-2">
            {remove.isError && (
              <p role="alert" className="text-xs text-red-400">
                {remove.error.message}
              </p>
            )}
            <div className="flex justify-end gap-2">
              <button type="button" className={button} onClick={() => change(false)}>
                Cancel
              </button>
              <button type="button" className={dangerButton} disabled={remove.isPending} onClick={() => remove.mutate(machine.id)}>
                Remove from fleet
              </button>
            </div>
          </div>
        </Modal>
      </div>
    </Section>
  );
}
