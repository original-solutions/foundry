import type { Step } from "./api";

/** What a session is doing right now, on one small line: a tool call, a question for you, or the prompt it took. */
export function Now({ step, className = "" }: { step: Step; className?: string }) {
  const look = step.kind === "ask" ? "text-amber-300" : step.kind === "tool" ? "font-mono text-[11px] text-neutral-400" : "text-neutral-500 italic";
  return (
    <span data-now={step.kind} className={`block truncate text-xs leading-4 ${look} ${className}`}>
      {step.kind === "prompt" ? `“${step.text}”` : step.text}
    </span>
  );
}

/** A terminal's name, then a session's first prompt when it has one. */
export function Named({ name, title }: { name: string; title?: string }) {
  if (!title) return <span className="min-w-0 flex-1 truncate">{name}</span>;
  return (
    <span className="min-w-0 flex-1 truncate">
      <span className="text-neutral-500">{name}</span> {title}
    </span>
  );
}
