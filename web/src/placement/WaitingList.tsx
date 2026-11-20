import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { cancelPlacement, listPlacements, placementsKey, type Placement } from "../api";
import { pollEvery } from "../poll";
import { useNow } from "../StepsPanel";

/** How often the list is read, so a request placed or failed shows within seconds. */
export const PLACEMENTS_EVERY = 2000;

/** How often the time each request has waited is shown again. */
const CLOCK_EVERY = 30_000;

function waited(since: number, now: number): string {
  const minutes = Math.floor((now - since) / 60_000);
  return minutes < 1 ? "waiting under a minute" : `waiting ${minutes} min`;
}

/**
 * The creates this machine holds while no machine qualifies for them: a card while any waits,
 * nothing otherwise. Each says why every machine was passed over and can be cancelled; one being
 * placed shows that, and one whose placement failed shows why until it is dismissed. A placed
 * request leaves the list and its workspace shows up as any other does.
 */
export function WaitingList() {
  const { data: requests = [] } = useQuery({
    queryKey: placementsKey,
    queryFn: listPlacements,
    refetchInterval: pollEvery(PLACEMENTS_EVERY, true),
    retry: false,
  });
  return requests.length > 0 ? <Requests requests={requests} /> : null;
}

/** Mounted while any request is held, so its clock starts from when the card shows. */
function Requests({ requests }: { requests: Placement[] }) {
  const client = useQueryClient();
  const cancel = useMutation({
    mutationFn: (request: Placement) => cancelPlacement(request.id),
    onSettled: () => client.invalidateQueries({ queryKey: placementsKey, exact: true }),
  });
  const now = useNow(CLOCK_EVERY);

  return (
    <section
      aria-label="Waiting for a machine"
      className="fixed inset-x-4 top-4 z-40 rounded-lg border border-sky-800/60 bg-neutral-900 text-sm shadow-lg shadow-black/50 md:inset-x-auto md:top-auto md:bottom-4 md:left-4 md:w-96"
    >
      <h2 className="px-3 py-2 font-medium text-sky-300">
        {requests.length} waiting for a machine
      </h2>
      <ul className="max-h-[40vh] divide-y divide-neutral-800 overflow-y-auto border-t border-neutral-800">
        {requests.map((request) => {
          const failed = request.state === "failed";
          const action = failed ? "Dismiss" : "Cancel";
          return (
            <li key={request.id} className="space-y-1 px-3 py-2">
              <p className="flex items-baseline gap-2">
                <span className="min-w-0 truncate font-medium text-neutral-100">{request.name}</span>
                <span className="shrink-0 text-xs text-neutral-500">
                  {failed ? "not placed" : request.state === "placing" ? "placing…" : waited(request.waiting_since, Math.max(now, request.waiting_since))}
                </span>
              </p>
              {failed ? (
                <p className="text-red-400">{request.error}</p>
              ) : (
                <ul className="text-neutral-400">
                  {request.reasons.map((reason) => (
                    <li key={reason}>{reason}</li>
                  ))}
                </ul>
              )}
              {request.state !== "placing" && (
                <p className="pt-1">
                  <button
                    type="button"
                    aria-label={`${action} ${request.name}`}
                    disabled={cancel.isPending && cancel.variables?.id === request.id}
                    onClick={() => cancel.mutate(request)}
                    className="text-sky-400 hover:text-sky-300 disabled:text-neutral-500"
                  >
                    {action}
                  </button>
                </p>
              )}
              {cancel.isError && cancel.variables?.id === request.id && (
                <p role="alert" className="text-red-400">
                  {cancel.error.message}
                </p>
              )}
            </li>
          );
        })}
      </ul>
    </section>
  );
}
