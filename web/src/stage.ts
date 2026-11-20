import type { Changes, Shipping } from "./api";

/** How far a workspace's work has gone on its way to its base: only on its machine, on GitHub, in a pull request, merged. */
export type Stage = "here" | "pushed" | "review" | "merged";

/** The stages in order, as the board names its lanes when grouped by shipping. */
export const stages: { stage: Stage; name: string; hint: string }[] = [
  { stage: "here", name: "Only here", hint: "Edits or commits GitHub does not have yet" },
  { stage: "pushed", name: "Pushed", hint: "The branch is on GitHub, with no open pull request" },
  { stage: "review", name: "In review", hint: "A pull request is open" },
  { stage: "merged", name: "Merged", hint: "Its pull request merged" },
];

/**
 * Where a workspace's work stands: the furthest it has gone. A merged or open pull request says
 * it outright; a branch on GitHub with commits of its own, or whose pull request closed, is pushed;
 * any edit or commit only this machine has is here; nothing at all is `none`. Read from the
 * workspace's changes and, when known, where its branch stands (`shipping`); work still to push
 * after a stage is a note on the card, not a step back.
 */
export function stageOf(changes: Changes, shipping?: Shipping): Stage | "none" {
  const pr = shipping?.pr;
  if (pr?.state === "merged") return "merged";
  if (pr?.state === "open") return "review";
  if (pr?.state === "closed") return "pushed";
  if (shipping?.pushed && changes.commits.length > 0) return "pushed";
  if (changes.commits.length > 0 || changes.files.length > 0) return "here";
  return "none";
}

/** A stage's place in the order, for telling a step forward from a step back. */
export const stageIndex = (stage: Stage) => stages.findIndex((s) => s.stage === stage);
