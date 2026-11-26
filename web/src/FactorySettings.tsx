import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState, type FormEvent } from "react";
import { saveFactorySettings, setProjectFactory, type FactorySettings, type Project, type ProjectFactory } from "./api";
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

/** A number as typed; blank or not a number is `NaN`, which the check refuses. */
const num = (text: string) => (text.trim() === "" ? NaN : Number(text));

/**
 * The fleet's factory settings as a form: pause claims, the model and effort a run's session
 * starts with, how many runs and gates at once, and how often it polls and waits for feedback.
 * Save checks every field against its allowed range first, and says what is wrong in the form
 * without sending; the daemon's own refusal shows there too. The fields follow `settings` as they
 * change, until something is typed.
 */
export function FleetSettings({ settings }: { settings: FactorySettings }) {
  const client = useQueryClient();
  const [values, setValues] = useState<Texts>(() => texts(settings));
  /** The settings the props held last, and the ones the fields were last made clean from (a save's answer may be ahead of the props). */
  const [seen, setSeen] = useState(settings);
  const [clean, setClean] = useState(settings);
  const [problem, setProblem] = useState<string | null>(null);
  // Settings that changed under an untouched form are shown; under a form being edited they wait.
  if (JSON.stringify(seen) !== JSON.stringify(settings)) {
    if (JSON.stringify(values) === JSON.stringify(texts(clean))) {
      setValues(texts(settings));
      setClean(settings);
    }
    setSeen(settings);
  }

  const save = useMutation({
    mutationFn: ({ next }: { next: FactorySettings; sent: Texts }) => saveFactorySettings(next),
    onSuccess: (saved, { sent }) => {
      // What the daemon holds now, so a number typed as "05" does not leave the form changed for good;
      // but typing since the send is kept.
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
   * The daemon's answer to the last accepted change, with the setting the props held when the
   * first change of the chain was sent (`base`) and the answers before it (`seen`), as JSON. Settings
   * that have not caught up come as a new object holding the same setting, so the props are
   * compared by content. It is dropped once the props hold the answer, or a setting that is
   * neither the base nor an earlier answer.
   */
  const [answered, setAnswered] = useState<{ value: ProjectFactory; base: string; seen: string[] }>();
  const now = JSON.stringify(factory);
  if (answered && (now === JSON.stringify(answered.value) || (now !== answered.base && !answered.seen.includes(now)))) setAnswered(undefined);
  const held = answered?.value ?? factory;
  const shown = typed ?? held.login;
  const send = useMutation({
    mutationFn: ({ next }: { next: { on: boolean; paused: boolean; login: string | null }; base: string; savingLogin: boolean; sentTyped?: string }) => setProjectFactory(project.id, next),
    onSuccess: (answer, { base, savingLogin, sentTyped }) => {
      // Typing is cleared when it is what was sent or the daemon now holds it; typing since stays.
      setTyped((t) => (t === undefined || (savingLogin && t === sentTyped) || t.trim() === answer.login ? undefined : t));
      setAnswered((prev) => (prev ? { value: answer, base: prev.base, seen: [...prev.seen, JSON.stringify(prev.value)] } : { value: answer, base, seen: [] }));
      return client.invalidateQueries({ queryKey: factoryKey });
    },
  });
  const change = (edit: Partial<ProjectFactory>, savingLogin = false) => {
    const next = { on: held.on, paused: held.paused, login: held.login, ...edit };
    send.mutate({ next: { on: next.on, paused: next.paused, login: next.login.trim() === "" ? null : next.login.trim() }, base: now, savingLogin, sentTyped: typed });
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
