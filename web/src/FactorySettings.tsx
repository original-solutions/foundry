import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState, type FormEvent } from "react";
import { HIDDEN, saveFactorySettings, setProjectFactory, type FactorySettings, type Project, type ProjectFactory } from "./api";
import { checkSettings, factoryKey, type NumericSetting } from "./factory";
import { button, field, inlineField, primaryButton } from "./Modal";

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

/** `kept` with each field `follows` picks taken from `next`. */
function following<T extends object>(kept: T, next: T, follows: (key: keyof T) => boolean): T {
  const out = { ...kept };
  for (const key of Object.keys(next) as (keyof T)[]) if (follows(key)) out[key] = next[key];
  return out;
}

/** A number as typed; blank or not a number is `NaN`, which the check refuses. */
const num = (text: string) => (text.trim() === "" ? NaN : Number(text));

/**
 * The fleet's factory settings as a form: pause claims, the model and effort a run's session
 * starts with, how many runs and gates at once, and how often it polls and waits for feedback.
 * Save checks every field against its allowed range first, and says what is wrong in the form
 * without sending; the daemon's own refusal shows there too. Each field follows `settings` as they
 * change, until something is typed in it.
 */
export function FleetSettings({ settings }: { settings: FactorySettings }) {
  const client = useQueryClient();
  const [values, setValues] = useState<Texts>(() => texts(settings));
  /** The settings the props held last, and the ones the fields were last made clean from (a save's answer may be ahead of the props). */
  const [seen, setSeen] = useState(settings);
  const [clean, setClean] = useState(settings);
  const [problem, setProblem] = useState<string | null>(null);
  // Settings that changed are shown in every field not being edited, so a save sends those as they
  // now are; a field being edited keeps what was typed.
  if (JSON.stringify(seen) !== JSON.stringify(settings)) {
    const was = texts(clean);
    const now = texts(settings);
    const follows = (key: keyof Texts) => values[key] === was[key] || values[key] === now[key];
    setValues(following(values, now, follows));
    setClean(following(clean, settings, follows));
    setSeen(settings);
  }

  const save = useMutation({
    mutationFn: ({ next }: { next: FactorySettings; sent: Texts }) => saveFactorySettings(next),
    onSuccess: (answer, { sent }) => {
      // What the daemon holds now, so a number typed as "05" does not leave the form changed for good;
      // but typing since the send is kept, and so is a word the daemon could not show.
      const saved = { ...answer, model: answer.model === HIDDEN ? sent.model : answer.model, effort: answer.effort === HIDDEN ? sent.effort : answer.effort };
      setValues((now) => (JSON.stringify(now) === JSON.stringify(sent) ? texts(saved) : now));
      setClean(saved);
      return client.invalidateQueries({ queryKey: factoryKey });
    },
  });

  const set = <K extends keyof Texts>(key: K, value: Texts[K]) => {
    setProblem(null);
    save.reset();
    setValues((v) => ({ ...v, [key]: value }));
  };

  const submit = (event: FormEvent) => {
    event.preventDefault();
    const next: FactorySettings = {
      paused: values.paused,
      model: values.model.trim(),
      effort: values.effort.trim(),
      max_runs: num(values.max_runs),
      max_gates: num(values.max_gates),
      poll_seconds: num(values.poll_seconds),
      feedback_seconds: num(values.feedback_seconds),
    };
    const wrong = checkSettings(next);
    setProblem(wrong);
    if (wrong === null) save.mutate({ next, sent: values });
  };

  const number = (label: string, key: NumericSetting) => (
    <label className="block text-xs text-neutral-400">
      {label}
      <input type="number" inputMode="numeric" value={values[key]} onChange={(e) => set(key, e.target.value)} className={field} />
    </label>
  );
  const word = (label: string, key: "model" | "effort") => (
    <label className="block text-xs text-neutral-400">
      {label}
      <input autoComplete="off" spellCheck={false} value={values[key]} onChange={(e) => set(key, e.target.value)} className={`${field} font-mono`} />
    </label>
  );

  return (
    <form onSubmit={submit} aria-label="Factory settings" className="grid gap-3 rounded-lg border border-neutral-800 p-3 sm:grid-cols-3">
      <label className="flex items-center gap-2 self-end text-xs text-neutral-300 sm:col-span-3">
        <input type="checkbox" checked={values.paused} onChange={(e) => set("paused", e.target.checked)} />
        Pause claims
      </label>
      {word("Model", "model")}
      {word("Effort", "effort")}
      {number("Max runs", "max_runs")}
      {number("Max gates", "max_gates")}
      {number("Poll seconds", "poll_seconds")}
      {number("Feedback seconds", "feedback_seconds")}
      {(problem ?? (save.isError ? save.error.message : null)) && (
        <p role="alert" className="text-xs text-red-400 sm:col-span-3">
          {problem ?? save.error?.message}
        </p>
      )}
      <div className="flex justify-end sm:col-span-3">
        <button type="submit" disabled={save.isPending} className={primaryButton}>
          Save
        </button>
      </div>
    </form>
  );
}

/** A project's factory setting as a key that does not depend on the order of a JSON object's fields. */
const keyOf = (f: ProjectFactory) => `${f.on}|${f.paused}|${f.login}`;

/**
 * One project's factory setting beside its row: a switch, the GitHub login it acts as (blank takes
 * the coordinator's default when turned on) with a Save login button once the typed login
 * differs, and a pause for its claims. Each change sends the whole setting, built from the
 * daemon's answer to the last change until the settings catch up over the events socket; the
 * switch and the pause send the saved login, and only Save login sends what is typed. A refusal
 * says why beside the row.
 */
export function ProjectFactoryControls({ project, factory }: { project: Project; factory: ProjectFactory }) {
  const client = useQueryClient();
  const [typed, setTyped] = useState<string>();
  /**
   * The daemon's answer to the last accepted change, and the keys of every setting the props may
   * pass through on their way to it: what they held when each change was sent or answered, and the
   * answers before. It is dropped once the props hold the answer, or a setting not among those.
   */
  const [answered, setAnswered] = useState<{ value: ProjectFactory; seen: string[] }>();
  const key = keyOf(factory);
  const props = useRef(key);
  useEffect(() => {
    props.current = key;
  }, [key]);
  if (answered && (key === keyOf(answered.value) || !answered.seen.includes(key))) setAnswered(undefined);
  const held = answered?.value ?? factory;
  const shown = typed ?? held.login;
  const send = useMutation({
    mutationFn: ({ next }: { next: { on: boolean; paused: boolean; login: string | null }; base: string; savingLogin: boolean; sentTyped?: string }) => setProjectFactory(project.id, next),
    onSuccess: (answer, { next, base, savingLogin, sentTyped }) => {
      // Typing is cleared when it is what was sent or the daemon now holds it; typing since stays.
      setTyped((t) => (t === undefined || (savingLogin && t === sentTyped) || t.trim() === answer.login ? undefined : t));
      // The daemon hides only strings when its secret store cannot be read: the switch and pause it
      // answers are what it holds, and the login is the one sent. A turn-on that named no login took
      // the coordinator's default, which the answer hides, so the login stays blank until the
      // settings arrive; a change sent before then names none and is refused, and the refusal shows.
      const value = answer.login === HIDDEN ? { ...answer, login: next.login ?? "" } : answer;
      setAnswered((prev) => ({ value, seen: [...new Set([...(prev?.seen ?? []), base, props.current, ...(prev ? [keyOf(prev.value)] : [])])] }));
      return client.invalidateQueries({ queryKey: factoryKey });
    },
  });
  const change = (edit: Partial<ProjectFactory>, savingLogin = false) => {
    const next = { on: held.on, paused: held.paused, login: held.login, ...edit };
    send.mutate({ next: { on: next.on, paused: next.paused, login: next.login.trim() === "" ? null : next.login.trim() }, base: key, savingLogin, sentTyped: typed });
  };

  return (
    <div className="mt-2 flex flex-wrap items-center gap-x-4 gap-y-2 text-xs text-neutral-300">
      <button
        type="button"
        role="switch"
        aria-checked={held.on}
        aria-label={`Factory for ${project.name}`}
        disabled={send.isPending}
        onClick={() => change({ on: !held.on })}
        className={`${button} ${held.on ? "border-emerald-400/50 text-emerald-200" : ""}`}
      >
        {held.on ? "Factory on" : "Factory off"}
      </button>
      <input
        aria-label={`GitHub login for ${project.name}`}
        autoComplete="off"
        spellCheck={false}
        placeholder="the coordinator's default"
        value={shown}
        onChange={(e) => setTyped(e.target.value)}
        className={`w-44 ${inlineField}`}
      />
      {typed !== undefined && typed.trim() !== held.login && (
        <button type="button" disabled={send.isPending} onClick={() => change({ login: typed }, true)} className={button}>
          Save login
        </button>
      )}
      <label className="flex items-center gap-1.5">
        <input type="checkbox" aria-label={`Pause claims for ${project.name}`} checked={held.paused} disabled={send.isPending} onChange={(e) => change({ paused: e.target.checked })} />
        Pause claims
      </label>
      {send.isError && (
        <p role="alert" className="basis-full text-red-400">
          {send.error.message}
        </p>
      )}
    </div>
  );
}
