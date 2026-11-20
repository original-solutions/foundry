import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { changesQuery } from "./ChangesPanel";
import type { Located } from "./fleet";
import { nextSteps, type NextStep } from "./next";
import { pollEvery } from "./poll";
import { CHECKS_EVERY, shippingQuery } from "./Shipping";

/** Next steps are offered to an idle session of this machine (`here`): one that waits for its next prompt, and whose workspace this daemon can read. */
export function offersNext(located: Located | undefined, here: string | undefined): boolean {
  return !!located?.session && located.glyph === "ready" && here !== undefined && located.machine.id === here;
}

const looks = {
  // Beside the terminal's name in its header, where a mouse is.
  header: {
    row: "flex shrink-0 items-center gap-1.5",
    chip: "h-7 rounded-full px-2.5 text-xs",
  },
  // Under the session's row on a board card.
  card: {
    row: "flex flex-wrap items-center gap-1.5",
    chip: "h-7 rounded-full px-2.5 text-xs",
  },
  // At the head of a phone's key bar, a thumb's size.
  keys: {
    row: "contents",
    chip: "h-9 shrink-0 rounded-md px-3 text-sm",
  },
};

/**
 * One-tap next steps for an idle session: what its workspace needs to get its work out (commit,
 * push and open a pull request, fix failing checks, answer a review), each sent to the session as
 * a prompt by `send`. Mounted when the session goes idle, so each turn's end reads the workspace's
 * changes and where its branch stands afresh; read again while checks run. A step pressed stays
 * down until the session starts working and the steps go away, or until the steps change; one
 * that did not reach it says so.
 */
export function NextSteps({ workspace, name, send, look }: { workspace: string; name: string; send: (prompt: string) => Promise<void> | void; look: keyof typeof looks }) {
  const { data: changes } = useQuery({ ...changesQuery(workspace), staleTime: 0, retry: false });
  const { data: shipping } = useQuery({
    ...shippingQuery(workspace),
    staleTime: 0,
    retry: false,
    refetchInterval: (query) => pollEvery(CHECKS_EVERY, query.state.data?.pr?.state === "open" && query.state.data.pr.checks === "pending")(query),
  });
  const steps = changes ? nextSteps(changes, shipping) : [];
  return <StepChips steps={steps} name={name} send={send} look={look} />;
}

/**
 * The steps `steps` as chips, each sent to session `name` as a prompt by `send`. A step pressed
 * stays down until the chips are mounted afresh (the session started working, so callers key or
 * unmount them) or the steps change, never on a timer: a session whose hooks are down still reads
 * as idle, and a step that pushes or opens a pull request must not be sent twice. One that did
 * not reach the session says so. Nothing shows for no steps.
 */
export function StepChips({ steps, name, send, look }: { steps: NextStep[]; name: string; send: (prompt: string) => Promise<void> | void; look: keyof typeof looks }) {
  // The steps a press was for: a press counts only while they are the steps shown.
  const offered = steps.map((s) => `${s.label}\n${s.prompt}`).join("\0");
  const [press, setPress] = useState<{ label: string; offered: string } | null>(null);
  const sent = press !== null && press.offered === offered ? press.label : null;
  const [failed, setFailed] = useState(false);
  if (steps.length === 0) return null;
  const pick = async (step: NextStep) => {
    setPress({ label: step.label, offered });
    setFailed(false);
    try {
      await send(step.prompt);
    } catch {
      setPress(null);
      setFailed(true);
    }
  };
  const { row, chip } = looks[look];
  return (
    <div role="group" aria-label={`Next for ${name}`} className={row}>
      <span className="flex shrink-0 items-center gap-1 pr-0.5 text-[10px] font-medium tracking-wide text-emerald-400 uppercase">
        <svg viewBox="0 0 16 16" className="size-3" fill="currentColor" aria-hidden="true">
          <path d="M8 1.5 9.4 6.6 14.5 8 9.4 9.4 8 14.5 6.6 9.4 1.5 8 6.6 6.6Z" />
        </svg>
        Next
      </span>
      {steps.map((step) => (
        <button
          key={step.label}
          type="button"
          title={`Ask ${name}: ${step.prompt}`}
          disabled={sent !== null}
          onPointerDown={(e) => e.preventDefault()}
          onClick={() => void pick(step)}
          className={`${chip} font-medium whitespace-nowrap ring-1 transition-colors active:scale-95 disabled:opacity-50 ${
            step.urgent
              ? "bg-red-400/10 text-red-300 ring-red-400/40 hover:bg-red-400/20"
              : "bg-emerald-400/10 text-emerald-300 ring-emerald-400/40 hover:bg-emerald-400/20"
          } ${sent === step.label ? "animate-pulse" : ""}`}
        >
          {step.label}
        </button>
      ))}
      {failed && (
        <span role="alert" title={`Could not reach ${name}`} className="shrink-0 text-xs text-amber-400">
          Not sent
        </span>
      )}
    </div>
  );
}
