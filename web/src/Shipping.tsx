import { useQuery } from "@tanstack/react-query";
import { workspaceShipping, type PullRequest, type Shipping } from "./api";
import { plural } from "./labels";
import { pollEvery } from "./poll";

/** The read every place that shows where a branch stands shares, one cache entry per workspace. */
export const shippingQuery = (workspace: string) => ({ queryKey: ["pr", workspace], queryFn: () => workspaceShipping(workspace) });

/** How often a pull request whose checks still run is read again while it is shown. */
export const CHECKS_EVERY = 60_000;

type Tone = "quiet" | "warn" | "bad" | "good" | "merged";

interface Note {
  text: string;
  tone: Tone;
}

/** What a line says about where a branch stands: its lead words, notes after them, and its link. */
export interface Line {
  lead: string;
  tone: Tone;
  notes: Note[];
  /** The pull request on GitHub. */
  pr?: PullRequest;
  /** Its pull request merged and nothing came after: the workspace has done its job. */
  done: boolean;
}

const checks: Record<PullRequest["checks"], Note | undefined> = {
  none: undefined,
  pending: { text: "Checks running", tone: "warn" },
  passing: { text: "Checks pass", tone: "good" },
  failing: { text: "Checks failing", tone: "bad" },
};

const review: Record<PullRequest["review"], Note | undefined> = {
  none: undefined,
  required: { text: "Review required", tone: "quiet" },
  approved: { text: "Approved", tone: "good" },
  changes_requested: { text: "Changes requested", tone: "warn" },
};

/**
 * Where a branch stands in words, or nothing for a branch that never left this machine and has
 * nothing to leave with. An open pull request takes the tone of its worst note, so a failing
 * check reads red from across the board.
 */
export function shipLine(s: Shipping): Line | undefined {
  const unpushed: Note[] = s.unpushed > 0 ? [{ text: `${plural(s.unpushed, "commit")} not pushed`, tone: "warn" }] : [];
  const { pr } = s;
  if (pr?.state === "merged") return { lead: `PR #${pr.number} merged`, tone: "merged", notes: unpushed, pr, done: unpushed.length === 0 };
  if (pr?.state === "closed") return { lead: `PR #${pr.number} closed`, tone: "quiet", notes: unpushed, pr, done: false };
  if (pr) {
    const notes = [checks[pr.checks], review[pr.review], ...unpushed].filter((n): n is Note => n !== undefined);
    const tone = notes.some((n) => n.tone === "bad") ? "bad" : notes.some((n) => n.tone === "warn") ? "warn" : notes.some((n) => n.tone === "good") ? "good" : "quiet";
    return { lead: `${pr.draft ? "Draft PR" : "PR"} #${pr.number}`, tone, notes, pr, done: false };
  }
  if (!s.pushed) {
    if (s.unpushed === 0) return undefined;
    return { lead: "Not pushed", tone: "quiet", notes: [{ text: `${plural(s.unpushed, "commit")} only here`, tone: "warn" }], done: false };
  }
  if (s.unreachable) return { lead: "Pushed", tone: "quiet", notes: [{ text: "GitHub not reachable", tone: "quiet" }, ...unpushed], done: false };
  return { lead: "Pushed", tone: "quiet", notes: [{ text: "No PR yet", tone: "quiet" }, ...unpushed], done: false };
}

const text: Record<Tone, string> = {
  quiet: "text-neutral-400",
  warn: "text-amber-300",
  bad: "text-red-400",
  good: "text-emerald-400",
  merged: "text-violet-300",
};

const PR_ICON = "M4 5.5v5M12 10.5V7a2.5 2.5 0 0 0-2.5-2.5H7m1.5-1.5L7 4.5 8.5 6";

/**
 * Where a workspace's branch stands past this machine, in one line: what is only here, whether
 * GitHub has it, and its pull request with checks and review, linked to GitHub. A merged pull
 * request with nothing after it offers to archive the workspace, where `onArchive` is given (this
 * machine's workspaces). Read when `enabled` turns on and when a caller marks `shippingQuery` out of date; again every
 * `CHECKS_EVERY` only while checks run, or every `every` when given. Shows nothing until there
 * is something to say, so a fresh workspace's card stays as it was.
 */
export function ShipLine({
  workspace,
  name,
  enabled,
  every,
  onArchive,
  className = "",
}: {
  workspace: string;
  name: string;
  enabled: boolean;
  every?: number;
  onArchive?: () => void;
  className?: string;
}) {
  const { data } = useQuery({
    ...shippingQuery(workspace),
    enabled,
    staleTime: 30_000,
    retry: false,
    refetchInterval: (query) => {
      const pending = query.state.data?.pr?.state === "open" && query.state.data.pr.checks === "pending";
      return pollEvery(every ?? CHECKS_EVERY, enabled && (every !== undefined || pending))(query);
    },
  });
  const line = data && shipLine(data);
  if (!line) return null;
  const lead = (
    <>
      <svg viewBox="0 0 16 16" className="size-3.5 shrink-0" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
        <circle cx="4" cy="3.5" r="1.5" />
        <circle cx="4" cy="12" r="1.5" />
        <circle cx="12" cy="12" r="1.5" />
        <path d={PR_ICON} />
      </svg>
      <span className="font-medium">{line.lead}</span>
    </>
  );
  return (
    <div className={`flex min-w-0 items-center gap-2 text-xs ${className}`}>
      {line.pr ? (
        <a
          href={line.pr.url}
          target="_blank"
          rel="noreferrer"
          title={`${line.pr.title} (open on GitHub)`}
          className={`flex shrink-0 items-center gap-1.5 rounded hover:underline hover:underline-offset-4 ${text[line.tone]}`}
        >
          {lead}
        </a>
      ) : (
        <span className={`flex shrink-0 items-center gap-1.5 ${text[line.tone]}`} title={`Branch ${data.branch}`}>
          {lead}
        </span>
      )}
      <span className="min-w-0 flex-1 truncate" title={[...line.notes.map((n) => n.text), line.pr?.title].filter(Boolean).join(" · ")}>
        {line.notes.map((n) => (
          <span key={n.text}>
            <span aria-hidden="true" className="text-neutral-600">
              {" · "}
            </span>
            <span className={text[n.tone]}>{n.text}</span>
          </span>
        ))}
        {line.pr && <span className="pl-1.5 text-neutral-500">{line.pr.title}</span>}
      </span>
      {line.done && onArchive && (
        <button
          type="button"
          onClick={onArchive}
          aria-label={`Archive ${name}: its pull request merged`}
          title="Its pull request merged: see what stays and what stops, then archive"
          className="shrink-0 rounded-md border border-violet-400/40 bg-violet-400/10 px-2 py-0.5 font-medium text-violet-200 transition-colors hover:bg-violet-400/20"
        >
          Archive
        </button>
      )}
    </div>
  );
}
