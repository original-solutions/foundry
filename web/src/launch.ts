import type { QueryClient } from "@tanstack/react-query";
import { ApiError, createWorkspace, placementsKey, startSession } from "./api";

/** The workspace was made, but its session did not start. */
export class SessionNotStarted extends ApiError {}

/**
 * What a new workspace is made from: its project, base, name and machine (absent for this
 * machine, "choose" to let Foundry pick one), then its session's model and first prompt.
 */
export interface CreateInput {
  project: string;
  base: string;
  name: string;
  machine?: string;
  model: string;
  prompt?: string;
  /** Names to try in turn while the daemon refuses the one before as taken (409): none for a typed name. */
  then?: string[];
}

/**
 * Makes a workspace, then starts a session in it. The workspace is made under the first of the
 * input's names (`name`, then `then`) the daemon does not refuse as taken, `onName` hearing each
 * before it is sent, and `onSession` once the session is being started. Letting Foundry choose
 * sends the model and first prompt along: when no machine qualifies the request waits (`session`
 * is null), and the waiting list is read again to show it, Foundry starting the session once it is
 * placed. A session that did not start throws `SessionNotStarted`, since the workspace exists and
 * a second try would only find its name taken.
 */
export async function makeWorkspace(
  input: CreateInput,
  client: QueryClient,
  { onName = () => {}, onSession = () => {} }: { onName?: (name: string) => void; onSession?: () => void } = {},
): Promise<{ session: string | null; warning: string | null }> {
  const session = input.machine === "choose" ? { model: input.model, prompt: input.prompt } : {};
  const names = [input.name, ...(input.then ?? [])];
  let workspace: Awaited<ReturnType<typeof createWorkspace>> | undefined;
  for (const [i, name] of names.entries()) {
    onName(name);
    try {
      workspace = await createWorkspace({ project: input.project, base: input.base, name, machine: input.machine, ...session });
      break;
    } catch (error) {
      // A refused try made nothing, so the next name is tried in its place.
      if (!(error instanceof ApiError && error.status === 409) || i === names.length - 1) throw error;
    }
  }
  if (!workspace) throw new Error("no workspace name to try");
  if ("request" in workspace) {
    await client.invalidateQueries({ queryKey: placementsKey });
    return { session: null, warning: null };
  }
  onSession();
  // The workspace is usable when its setup did not start, so its session is started all the same.
  let started: string;
  try {
    started = await startSession(workspace.id, input.model, input.prompt);
  } catch (error) {
    const also = [workspace.error, workspace.warning].filter(Boolean).map((said) => ` Also: ${said}`).join("");
    throw new SessionNotStarted(`The workspace was made, but its session did not start: ${error instanceof Error ? error.message : error}.${also}`);
  }
  return { session: started, warning: [workspace.error, workspace.warning].filter(Boolean).join(" ") || null };
}
