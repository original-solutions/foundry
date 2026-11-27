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
}

/**
 * Makes a workspace, then starts a session in it, calling `onSession` between the two. Letting
 * Foundry choose sends the model and first prompt along: when no machine qualifies the request
 * waits (`session` is null), and the waiting list is read again to show it, Foundry starting the
 * session once it is placed. A session that did not start throws `SessionNotStarted`, since the
 * workspace exists and a second try would only find its name taken.
 */
export async function makeWorkspace(input: CreateInput, client: QueryClient, onSession = () => {}): Promise<{ session: string | null; warning: string | null }> {
  const session = input.machine === "choose" ? { model: input.model, prompt: input.prompt } : {};
  const workspace = await createWorkspace({ project: input.project, base: input.base, name: input.name, machine: input.machine, ...session });
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
