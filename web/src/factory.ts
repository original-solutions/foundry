import { createContext, useContext } from "react";
import type { FactoryOverview, FactorySettings, FactoryStatus, ProjectFactory } from "./api";

/** The react-query key of the Factory view's overview. */
export const factoryKey = ["factory"] as const;

/** Each project's factory setting as the settings last sent it; `undefined` where nothing provides it. */
export const FactoryContext = createContext<Record<string, ProjectFactory> | undefined>(undefined);

/** Each project's factory setting by project id, or `undefined` when nothing provides it. */
export function useProjectFactory(): Record<string, ProjectFactory> | undefined {
  return useContext(FactoryContext);
}

/** The settings that are whole numbers. */
export type NumericSetting = "poll_seconds" | "feedback_seconds" | "max_runs" | "max_gates";

/** The whole numbers the daemon takes for each number setting: the poll's range is the spec's, the rest are as wide as the daemon's integers. */
export const SETTING_RANGES: Record<NumericSetting, { min: number; max: number }> = {
  poll_seconds: { min: 10, max: 60 },
  feedback_seconds: { min: 0, max: Number.MAX_SAFE_INTEGER },
  max_runs: { min: 0, max: 4294967295 },
  max_gates: { min: 0, max: 4294967295 },
};

/** The first thing wrong with `values`, as a message that names the field and what is allowed, or `null` when all is well. */
export function checkSettings(values: FactorySettings): string | null {
  for (const name of Object.keys(SETTING_RANGES) as NumericSetting[]) {
    const { min, max } = SETTING_RANGES[name];
    const value = values[name];
    if (!Number.isInteger(value) || value < min || value > max) return `${name} must be a whole number from ${min} to ${max}`;
  }
  for (const name of ["model", "effort"] as const) {
    const value = values[name];
    if (value === "" || /\s/.test(value) || value.startsWith("-")) return `${name} is one word that does not start with "-"`;
  }
  return null;
}

/** A run id taken apart, as the daemon's `RunId` parses it. */
export interface Repo {
  owner: string;
  repo: string;
}
export interface RunParts extends Repo {
  issue: number;
  attempt: number;
}

const count = /^(0|[1-9][0-9]*)$/;

/** `<owner>__<repo>__<issue>__<attempt>`: the owner ends at the first `__`, the numbers are the last two fields. */
export function parseRun(id: string): RunParts | undefined {
  const first = id.indexOf("__");
  if (first < 0) return undefined;
  const owner = id.slice(0, first);
  const rest = id.slice(first + 2);
  const last = rest.lastIndexOf("__");
  if (last < 0) return undefined;
  const attempt = rest.slice(last + 2);
  const head = rest.slice(0, last);
  const middle = head.lastIndexOf("__");
  if (middle < 0) return undefined;
  const issue = head.slice(middle + 2);
  const repo = head.slice(0, middle);
  if (!owner || !repo || !count.test(issue) || !count.test(attempt)) return undefined;
  return { owner, repo, issue: Number(issue), attempt: Number(attempt) };
}

/** Where an issue of `owner/repo` lives on GitHub. */
export function issueUrl(of: Repo & { issue: number }): string {
  return `https://github.com/${of.owner}/${of.repo}/issues/${of.issue}`;
}

/** The GitHub owner and repo a project's repo URL names, when it is an https one; any user or hidden password is ignored. */
export function githubRepo(url: string): Repo | undefined {
  const match = /^https:\/\/(?:[^@/]*@)?github\.com\/([^/]+)\/([^/]+?)(?:\.git)?\/?$/.exec(url.trim());
  return match ? { owner: match[1], repo: match[2] } : undefined;
}

/**
 * What a project's head says, a line each: off, no status yet, or what its holder is doing and why
 * it is idle. The holder is named only when the overview's own status for the project holds at the
 * same generation, since only then is its `writer` the machine that holds it. `name` gives a
 * machine's name from its id.
 */
export function projectLine(entry: FactoryOverview["projects"][number], status: FactoryStatus | undefined, name: (machine: string) => string): string[] {
  if (!entry.factory.on) return ["Factory off"];
  if (!status) return ["No status yet"];
  if (status.holds) {
    const known = entry.status?.holds && entry.status.generation === status.generation ? entry.writer : null;
    const lines = [`Holds the lease at generation ${status.generation}${known ? `, on ${name(known)}` : ""}`];
    if (status.idle) lines.push(`Idle: ${status.idle}`);
    return lines;
  }
  const why = status.not_holding;
  if (why?.kind === "no_token") return [`Idle: this coordinator holds no token for ${why.login}`];
  if (why?.kind === "held_by") return [`Idle here: ${name(why.machine)} holds the lease at generation ${why.generation}`];
  return [status.idle ? `Idle: ${status.idle}` : "Idle"];
}
