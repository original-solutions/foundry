import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState, type FormEvent } from "react";
import { saveFactorySettings, setProjectFactory, type FactorySettings, type Project, type ProjectFactory } from "./api";
import { checkSettings, factoryKey } from "./factory";
import { button, field, primaryButton } from "./Modal";

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
  const [held, setHeld] = useState(settings);
  const [problem, setProblem] = useState<string | null>(null);
  // Settings that changed under an untouched form are shown; under a form being edited they wait.
  if (JSON.stringify(held) !== JSON.stringify(settings)) {
    if (JSON.stringify(values) === JSON.stringify(texts(held))) setValues(texts(settings));
    setHeld(settings);
  }

  const save = useMutation({
    mutationFn: saveFactorySettings,
    onSuccess: () => client.invalidateQueries({ queryKey: factoryKey }),
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
    if (wrong === null) save.mutate(next);
  };

  const number = (label: string, key: "max_runs" | "max_gates" | "poll_seconds" | "feedback_seconds") => (
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
 * differs, and a pause for its claims. Each change sends the whole setting; a refusal says why
 * beside the row. The new setting arrives back over the events socket.
 */
export function ProjectFactoryControls({ project, factory }: { project: Project; factory: ProjectFactory }) {
  const client = useQueryClient();
  const [typed, setTyped] = useState<string>();
  const shown = typed ?? factory.login;
  const send = useMutation({
    mutationFn: (next: { on: boolean; paused: boolean; login: string | null }) => setProjectFactory(project.id, next),
    onSuccess: () => {
      setTyped(undefined);
      return client.invalidateQueries({ queryKey: factoryKey });
    },
  });
  const change = (edit: Partial<ProjectFactory>) => {
    const next = {
      on: factory.on,
      paused: factory.paused,
      login: shown,
      ...edit,
    };
    send.mutate({
      on: next.on,
      paused: next.paused,
      login: next.login.trim() === "" ? null : next.login.trim(),
    });
  };

  return (
    <div className="mt-2 flex flex-wrap items-center gap-x-4 gap-y-2 text-xs text-neutral-300">
      <button
        type="button"
        role="switch"
        aria-checked={factory.on}
        aria-label={`Factory for ${project.name}`}
        disabled={send.isPending}
        onClick={() => change({ on: !factory.on })}
        className={`${button} ${factory.on ? "border-emerald-400/50 text-emerald-200" : ""}`}
      >
        {factory.on ? "Factory on" : "Factory off"}
      </button>
      <input
        aria-label={`GitHub login for ${project.name}`}
        autoComplete="off"
        spellCheck={false}
        placeholder="the coordinator's default"
        value={shown}
        onChange={(e) => setTyped(e.target.value)}
        className="w-44 rounded border border-neutral-600 bg-neutral-950 px-1.5 py-0.5 font-mono text-neutral-100 placeholder:text-neutral-600 focus:border-neutral-400"
      />
      {typed !== undefined && typed.trim() !== factory.login && (
        <button type="button" disabled={send.isPending} onClick={() => change({})} className={button}>
          Save login
        </button>
      )}
      <label className="flex items-center gap-1.5">
        <input type="checkbox" aria-label={`Pause claims for ${project.name}`} checked={factory.paused} disabled={send.isPending} onChange={(e) => change({ paused: e.target.checked })} />
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
