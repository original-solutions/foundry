import { useQuery } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { changesQuery } from "./ChangesPanel";
import type { Located } from "./fleet";
import { nextSteps, type NextStep } from "./next";
import { pollEvery } from "./poll";
import { CHECKS_EVERY, shippingQuery } from "./Shipping";

/** How long a pressed step stays down when the session does not start working, as when its hooks are not heard. */
export const SENT_FOR = 15_000;

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
 * down until the session starts working and the steps go away, or for `SENT_FOR` if it does not
 * seem to; one that did not reach it says so.
 */
export function NextSteps({ workspace, name, send, look }: { workspace: string; name: string; send: (prompt: string) => Promise<void> | void; look: keyof typeof looks }) {
  const { data: changes } = useQuery({ ...changesQuery(workspace), staleTime: 0, retry: false });
  const { data: shipping } = useQuery({
    ...shippingQuery(workspace),
    staleTime: 0,
    retry: false,
    refetchInterval: (query) => pollEvery(CHECKS_EVERY, query.state.data?.pr?.state === "open" && query.state.data.pr.checks === "pending")(query),
  });
  const [sent, setSent] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    if (sent === null) return;
    const timer = setTimeout(() => setSent(null), SENT_FOR);
    return () => clearTimeout(timer);
  }, [sent]);
  const steps = changes ? nextSteps(changes, shipping) : [];
  if (steps.length === 0) return null;
  const pick = async (step: NextStep) => {
    setSent(step.label);
    setFailed(false);
    try {
      await send(step.prompt);
    } catch {
      setSent(null);
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
