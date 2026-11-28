import { Link } from "@tanstack/react-router";
import type { MachineView } from "./fleet";
import { plural } from "./labels";
import { newestFirst } from "./UpdateChecklist";

/**
 * The release waiting on the fleet, at the head of the main pane (so a phone shows it with the
 * sidebar closed): each waiting tag and how many listed machines wait on it, as a link to
 * Settings → Machines, where the checklist installs it. Nothing shows while no listed machine has
 * a release waiting. `here` is not read: the checklist, not the badge, installs.
 */
export function UpdateBadge({ machines }: { machines: MachineView[]; here?: string }) {
  const waiting = machines.flatMap((m) => (m.listed && m.sample?.update_ready ? [m.sample.update_ready] : []));
  if (waiting.length === 0) return null;

  const counts = new Map<string, number>();
  for (const tag of waiting) counts.set(tag, (counts.get(tag) ?? 0) + 1);
  const groups = [...counts].sort(([a], [b]) => newestFirst(a, b));
  const summary = groups.map(([tag, n], i) => `${tag} on ${i === 0 ? plural(n, "machine") : n}`).join(", ");

  return (
    <div className="flex shrink-0 justify-end px-2 pt-2">
      <Link
        to="/settings/$category"
        params={{ category: "machines" }}
        className="flex max-w-full items-center gap-2 rounded-full border border-sky-400/40 bg-sky-400/10 px-3 py-1 text-xs text-sky-200 hover:bg-sky-400/20"
      >
        <span aria-hidden="true" className="size-1.5 shrink-0 rounded-full bg-sky-300" />
        <span className="min-w-0 truncate">Release waiting: {summary}</span>
      </Link>
    </div>
  );
}
