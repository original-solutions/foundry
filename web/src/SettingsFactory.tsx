import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useId, useState, type FC } from "react";
import { factoryOverview, HIDDEN, saveFactorySettings, type FactoryOverview, type FactorySettings } from "./api";
import { checkSettings, factoryKey } from "./factory";
import { field } from "./Modal";
import { diff, type Field, type Refusal } from "./reviewDiff";
import { ReviewBar, useLeaveGuard } from "./ReviewBar";
import { Page, Row, type PageProps } from "./SettingsPage";

/** The form's fields as text, so a half-typed number is kept as typed. */
function texts(s: FactorySettings) {
  return {
    paused: s.paused,
    model: s.model,
    effort: s.effort,
    max_runs: String(s.max_runs),
    max_gates: String(s.max_gates),
    poll_seconds: String(s.poll_seconds),
    feedback_seconds: String(s.feedback_seconds),
  };
}
type Texts = ReturnType<typeof texts>;

/** A number as typed; blank or not a number is `NaN`, which the check refuses. */
const num = (text: string) => (text.trim() === "" ? NaN : Number(text));

/** What each row shows: the label the review bar names it by, and the label today's form gave its control. */
interface Def extends Field<Texts> {
  /** What the control is named for a screen reader, when not its row's label. */
  name?: string;
  help: string;
  kind: "switch" | "word" | "number";
}
const defs: Def[] = [
  { key: "paused", label: "Pause claims", help: "Stop every project from claiming new issues. Runs already going carry on.", kind: "switch", show: (v) => (v ? "on" : "off") },
  { key: "model", label: "Model", help: "The model a run's session starts with.", kind: "word" },
  { key: "effort", label: "Effort", help: "The effort a run's session starts with.", kind: "word" },
  { key: "max_runs", label: "Runs at once", name: "Max runs", help: "How many runs build at the same time.", kind: "number" },
  { key: "max_gates", label: "Gates at once", name: "Max gates", help: "How many runs check their work at the same time.", kind: "number" },
  { key: "poll_seconds", label: "Poll interval", name: "Poll seconds", help: "How often it looks for issues to claim, from 10 to 60 seconds.", kind: "number" },
  { key: "feedback_seconds", label: "Feedback interval", name: "Feedback seconds", help: "How long it waits between looks for feedback on a pull request.", kind: "number" },
];

const without = <T extends object>(from: T, key: keyof T): T => {
  const out = { ...from };
  delete out[key];
  return out;
};

/** The edits that `sent` did not carry as they are now: typing since a send is kept. */
function unsent(now: Partial<Texts>, sent: Partial<Texts>): Partial<Texts> {
  const out: Partial<Texts> = { ...now };
  for (const key of Object.keys(sent) as (keyof Texts)[]) if (now[key] === sent[key]) delete out[key];
  return out;
}

/**
 * Settings → Factory: the fleet's factory settings, as the Factory view's fold used to hold them.
 * Edits collect in the review bar, each as old → new, and one Save checks every field against
 * its allowed range first and then sends one whole object built from the newest overview plus the
 * edits. Fields not being edited follow the overview as it changes; when it changes a field that
 * is being edited, the row says so and Save asks for a fresh look first.
 */
export const SettingsFactory: FC<PageProps> = () => {
  const { data: overview, error } = useQuery({ queryKey: factoryKey, queryFn: factoryOverview, retry: false });
  return (
    <Page title="Factory">
      {error && !overview && (
        <p role="alert" className="text-sm text-red-400">
          {error.message}
        </p>
      )}
      {!overview && !error && <p className="text-sm text-neutral-500">Loading…</p>}
      {overview && <Form saved={overview.settings} />}
    </Page>
  );
};

function Form({ saved: overview }: { saved: FactorySettings }) {
  const client = useQueryClient();
  const id = useId();
  /**
   * What was typed, by field (only the fields touched; the rest are the overview's), and what the
   * overview held for each edited field when it was last edited or looked at, to tell that it
   * moved since. Together, so a save settles both in one step.
   */
  const [draft, setDraft] = useState<{ edits: Partial<Texts>; from: Partial<Texts>; sending?: Partial<Texts> }>({ edits: {}, from: {} });
  const { edits, from } = draft;
  const [refusals, setRefusals] = useState<Refusal[]>([]);
  // A model or effort the daemon could not show (its secret store is unreadable) is kept as last read or sent.
  const [known, setKnown] = useState({ model: overview.model === HIDDEN ? "" : overview.model, effort: overview.effort === HIDDEN ? "" : overview.effort });
  // Adopted when the overview changes, not on every render, so a word just sent is not put back by the overview it replaces.
  const [seen, setSeen] = useState({ model: overview.model, effort: overview.effort });
  if (overview.model !== seen.model || overview.effort !== seen.effort) {
    setSeen({ model: overview.model, effort: overview.effort });
    setKnown({ model: overview.model === HIDDEN ? known.model : overview.model, effort: overview.effort === HIDDEN ? known.effort : overview.effort });
  }
  const saved: FactorySettings = { ...overview, model: overview.model === HIDDEN ? known.model : overview.model, effort: overview.effort === HIDDEN ? known.effort : overview.effort };

  const now = texts(saved);
  const values: Texts = { ...now, ...edits };
  const changes = diff(now, values, defs);
  const moved = new Set(diff({ ...now, ...from }, now, defs).map((c) => c.key));
  const stale = changes.map((c) => c.key).filter((key) => moved.has(key));
  useLeaveGuard(changes.length > 0);

  const set = <K extends keyof Texts>(key: K, value: Texts[K]) => {
    setRefusals((r) => r.filter((x) => x.key !== key));
    // Put back to what the overview holds, it is untouched again and follows it.
    // A field being sent is kept even then: what the save makes the overview decides whether it is a change.
    const same = diff(now, { ...now, [key]: value }, defs).length === 0;
    setDraft((d) => (same && !(key in (d.sending ?? {})) ? { ...d, edits: without(d.edits, key), from: without(d.from, key) } : { ...d, edits: { ...d.edits, [key]: value }, from: { ...d.from, [key]: now[key] } }));
  };

  const discard = () => {
    setDraft({ edits: {}, from: {} });
    setRefusals([]);
  };

  /** Takes what the overview holds now as seen for every stale field, so each is marked again only if it moves again. */
  const look = () => setDraft((d) => ({ ...d, from: { ...d.from, ...Object.fromEntries(stale.map((key) => [key, now[key as keyof Texts]])) } }));

  const save = async (): Promise<Refusal[]> => {
    const next: FactorySettings = {
      paused: values.paused,
      model: values.model.trim(),
      effort: values.effort.trim(),
      max_runs: num(values.max_runs),
      max_gates: num(values.max_gates),
      poll_seconds: num(values.poll_seconds),
      feedback_seconds: num(values.feedback_seconds),
    };
    const refuse = (found: Refusal[]) => {
      setRefusals(found);
      return found;
    };
    const wrong = checkSettings(next);
    // The check's message begins with the name of the field it refuses.
    if (wrong !== null) return refuse([{ key: defs.find((d) => wrong.startsWith(`${d.key} `))?.key, message: wrong }]);
    const sent = { ...edits };
    setDraft((d) => ({ ...d, sending: sent }));
    try {
      const answer = await saveFactorySettings(next);
      // A word the daemon could not show is kept as it was sent.
      const settings = { ...answer, model: answer.model === HIDDEN ? next.model : answer.model, effort: answer.effort === HIDDEN ? next.effort : answer.effort };
      // Set here too: a re-read that hides the word can land before the overview shows the one just sent.
      setKnown({ model: settings.model, effort: settings.effort });
      client.setQueryData<FactoryOverview>(factoryKey, (old) => old && { ...old, settings });
      const answered = texts(settings);
      setDraft((d) => {
        // Typing since the send is kept, and is measured from what the save made the overview.
        const kept = unsent(d.edits, sent);
        const keptFrom: Partial<Texts> = {};
        for (const key of Object.keys(kept) as (keyof Texts)[]) (keptFrom as Record<string, unknown>)[key] = key in sent ? answered[key] : d.from[key];
        return { edits: kept, from: keptFrom };
      });
      void client.invalidateQueries({ queryKey: factoryKey });
      return refuse([]);
    } catch (e) {
      setDraft((d) => ({ edits: d.edits, from: d.from }));
      return refuse([{ message: e instanceof Error ? e.message : "The settings were not saved." }]);
    }
  };

  return (
    <>
      <div>
        {defs.map((def) => {
          const key = def.key;
          const control = `${id}-${key}`;
          const name = def.name !== undefined && def.name !== def.label ? { "aria-label": def.name } : {};
          const note = refusals.find((r) => r.key === key);
          const isStale = stale.includes(key);
          const shown = def.show ? def.show(now[key]) : now[key];
          return (
            <Row
              key={key}
              label={def.label}
              help={def.help}
              htmlFor={control}
              changed={changes.some((c) => c.key === key)}
              stale={isStale}
              note={
                (note || isStale) && (
                  <p className={`mt-1 text-xs ${note ? "text-red-400" : "text-amber-300"}`}>
                    {note ? note.message : `This changed elsewhere, to ${String(shown)}, since you edited it.`}
                  </p>
                )
              }
            >
              {def.kind === "switch" ? (
                <input id={control} type="checkbox" checked={values.paused} onChange={(e) => set("paused", e.target.checked)} {...name} />
              ) : def.kind === "number" ? (
                <input id={control} type="number" inputMode="numeric" value={values[key] as string} onChange={(e) => set(key, e.target.value)} className={field} {...name} />
              ) : (
                <input id={control} autoComplete="off" spellCheck={false} value={values[key] as string} onChange={(e) => set(key, e.target.value)} className={`${field} font-mono`} {...name} />
              )}
            </Row>
          );
        })}
      </div>
      <ReviewBar changes={changes} onSave={save} onDiscard={discard} stale={stale} onLook={look} />
    </>
  );
}
