import type { Changes, Shipping } from "./api";
import { plural } from "./labels";

/** A step that takes a workspace's work further, and the prompt that asks its session to take it. */
export interface NextStep {
  label: string;
  prompt: string;
  /** What it fixes is broken: failing checks. */
  urgent?: boolean;
}

const OPEN_PR = "Push this branch and open a pull request for it with gh, with a short title and a summary of the change.";

/**
 * What a session could be asked next to get its workspace's work out: commit what is
 * uncommitted, push and open a pull request, fix its failing checks, answer its review, or push
 * what the pull request lacks. Read from the workspace's changes and, once read, where its branch
 * stands (`shipping`). Nothing that would leave uncommitted work behind is offered, nor a pull
 * request for a branch with no commits of its own, nor anything once its pull request merged or
 * closed.
 */
export function nextSteps(changes: Changes, shipping?: Shipping): NextStep[] {
  const steps: NextStep[] = [];
  const uncommitted = changes.files.filter((f) => f.uncommitted).length;
  if (uncommitted > 0) steps.push({ label: `Commit ${plural(uncommitted, "file")}`, prompt: "Commit the uncommitted changes, with a message that says what they do." });
  if (!shipping) return steps;
  const { pr } = shipping;
  if (pr) {
    if (pr.state !== "open") return steps;
    if (pr.checks === "failing") steps.push({ label: "Fix failing checks", prompt: "The checks on this branch's pull request are failing. Find out why with gh, fix it, commit and push.", urgent: true });
    if (pr.review === "changes_requested") steps.push({ label: "Address the review", prompt: "Read the review comments on this branch's pull request with gh, address them, commit and push." });
    if (uncommitted === 0 && shipping.unpushed > 0) steps.push({ label: `Push ${plural(shipping.unpushed, "commit")}`, prompt: "Push this branch." });
    return steps;
  }
  if (uncommitted > 0 || changes.commits.length === 0) return steps;
  if (shipping.unreachable) {
    if (shipping.unpushed > 0) steps.push({ label: `Push ${plural(shipping.unpushed, "commit")}`, prompt: "Push this branch." });
    return steps;
  }
  if (!shipping.pushed && shipping.unpushed > 0) steps.push({ label: "Push and open a PR", prompt: OPEN_PR });
  else if (shipping.pushed) steps.push({ label: "Open a PR", prompt: OPEN_PR });
  return steps;
}
