import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { factoryOverview, stopRun, type FactoryStatus, type FactoryWaiting, type FinishedRun, type HeldUp, type LiveRun, type SharedSettings } from "./api";
import { factoryKey, githubRepo, issueUrl, parseRun, projectLine, type Repo } from "./factory";
import { FleetSettings } from "./FactorySettings";
import { rows, type MachineView } from "./fleet";
import { button, dangerButton } from "./Modal";

/** How many finished runs a project shows. */
const FINISHED_SHOWN = 20;

/** `#N`, a link to the issue when its repo is known. */
function Issue({ number, repo }: { number: number; repo?: Repo }) {
  if (!repo) return <span>#{number}</span>;
  return (
    <a href={issueUrl({ ...repo, issue: number })} target="_blank" rel="noreferrer" className="text-sky-300 hover:underline">
      #{number}
    </a>
  );
}

function List({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="mt-3">
      <h3 className="text-xs font-medium uppercase tracking-wide text-neutral-500">{label}</h3>
      <ul aria-label={label} className="mt-1 space-y-1 text-sm text-neutral-300">
        {children}
      </ul>
    </div>
  );
}

function Waiting({ items, label, repo }: { items: FactoryWaiting[]; label: string; repo?: Repo }) {
  if (items.length === 0) return null;
  return (
    <List label={label}>
      {items.map((w) => (
        <li key={`${w.issue}:${w.run ?? ""}`}>
          <Issue number={w.issue} repo={(w.run ? parseRun(w.run) : undefined) ?? repo} /> {w.title} <span className="text-neutral-500">— {w.reason}</span>
        </li>
      ))}
    </List>
  );
}

/** Where a run's workspace is: a link to a terminal of its newest workspace some machine holds. */
function WorkspaceLink({ run, machines }: { run: LiveRun; machines: MachineView[] }) {
  const all = machines.flatMap((m) => m.workspaces);
  const id = [...run.workspaces].reverse().find((w) => all.some((x) => x.id === w));
  const workspace = all.find((w) => w.id === id);
  if (!workspace) return <span className="text-neutral-500">workspace not here yet</span>;
  const list = rows(workspace);
  const terminal = (list.find((r) => r.session !== undefined && run.sessions.includes(r.session)) ?? list[0])?.terminal;
  if (!terminal) return <span>{workspace.name}</span>;
  return (
    <Link to="/terminals/$terminalId" params={{ terminalId: terminal }} className="text-sky-300 hover:underline">
      {workspace.name}
    </Link>
  );
}

/** A Stop button that asks once more before it sends, then says what the daemon answered. */
function Stop({ run, issue }: { run: string; issue: number }) {
  const [asking, setAsking] = useState(false);
  const stop = useMutation({
    mutationFn: stopRun,
    onSettled: () => setAsking(false),
  });
  return (
    <span className="inline-flex flex-wrap items-center gap-2">
      {asking ? (
        <>
          <span className="text-xs text-amber-200">Stop run #{issue}? It goes stuck on the holder&apos;s next pass.</span>
          <button type="button" onClick={() => setAsking(false)} className={button}>
            Cancel
          </button>
          <button type="button" onClick={() => stop.mutate(run)} disabled={stop.isPending} className={dangerButton}>
            Stop the run
          </button>
        </>
      ) : (
        <button
          type="button"
          aria-label={`Stop #${issue}`}
          onClick={() => {
            stop.reset();
            setAsking(true);
          }}
          className={button}
        >
          Stop
        </button>
      )}
      {stop.isSuccess && <span className="text-xs text-emerald-300">{stop.data}</span>}
      {stop.isError && (
        <span role="alert" className="text-xs text-red-400">
          {stop.error.message}
        </span>
      )}
    </span>
  );
}

function Live({ run, machines, name }: { run: LiveRun; machines: MachineView[]; name: (id: string) => string }) {
  const parts = parseRun(run.run);
  return (
    <li className="flex flex-wrap items-center gap-x-3 gap-y-1">
      <span>
        {parts ? <Issue number={parts.issue} repo={parts} /> : run.run} {run.title}
      </span>
      <span className="text-neutral-500">{run.step}</span>
      <span>{name(run.machine)}</span>
      <span>attempt {parts?.attempt ?? "?"}</span>
      <span>gate retries {run.gate_retries_used}</span>
      <span>{run.minutes_used} min</span>
      <WorkspaceLink run={run} machines={machines} />
      {parts && <Stop run={run.run} issue={parts.issue} />}
    </li>
  );
}

function Held({ item }: { item: HeldUp }) {
  const parts = parseRun(item.run);
  return (
    <li>
      {parts ? <Issue number={parts.issue} repo={parts} /> : item.run} <span className="text-amber-200">{item.kind === "stuck" ? "stuck" : "needs info"}</span>{" "}
      <span className="text-neutral-500">— {item.reason}</span>
      {item.kind === "stuck" && <p className="text-xs text-neutral-500">Its workspace is kept until the issue leaves stuck; any uncommitted work there is lost then.</p>}
    </li>
  );
}

function Done({ item }: { item: FinishedRun }) {
  const parts = parseRun(item.run);
  return (
    <li>
      {parts ? <Issue number={parts.issue} repo={parts} /> : item.run} {item.title}{" "}
      <span className="text-neutral-500">
        — {item.outcome}
        {item.pr_end ? `, ${item.pr_end}` : ""}
      </span>
      {item.pr_url && (
        <>
          {" "}
          <a href={item.pr_url} target="_blank" rel="noreferrer" className="text-sky-300 hover:underline">
            PR
          </a>
        </>
      )}
    </li>
  );
}

/** What one project's factory is doing, from the status. */
function Body({ status, repo, machines, name }: { status: FactoryStatus; repo?: Repo; machines: MachineView[]; name: (id: string) => string }) {
  return (
    <>
      {status.owner_ignored && <p className="mt-1 text-xs text-amber-200">This repo sets factory.owner, which Foundry ignores: the lease decides which machine acts.</p>}
      <Waiting items={status.ready} label="Ready" repo={repo} />
      <Waiting items={status.feedback_waiting} label="Feedback waiting" repo={repo} />
      {status.runs.length > 0 && (
        <List label="Live runs">
          {status.runs.map((run) => (
            <Live key={run.run} run={run} machines={machines} name={name} />
          ))}
        </List>
      )}
      {status.held_up.length > 0 && (
        <List label="Held up">
          {status.held_up.map((item) => (
            <Held key={item.run} item={item} />
          ))}
        </List>
      )}
      {status.finished.length > 0 && (
        <List label="Finished">
          {status.finished.slice(0, FINISHED_SHOWN).map((item) => (
            <Done key={item.run} item={item} />
          ))}
        </List>
      )}
    </>
  );
}

/**
 * The Factory view: the fleet's factory settings in a form at the head, and for each project what
 * its lease holder says (or why it is idle), the ready queue with each issue's reason, feedback
 * rounds that wait, live runs each with a link to its workspace and a Stop that asks first, runs
 * held up, and the last 20 finished. The statuses the events socket sends win over what the
 * overview read. The overview is read again whenever `statuses` changes (a state message resets
 * it, and each status after is a new one) or the settings' seq does, so its writer and settings
 * follow the socket and a failed read heals on the next one. Claims by another factory are not
 * listed here; they are an attention item.
 */
export function Factory({ statuses, settings, machines }: { statuses: Record<string, FactoryStatus> | undefined; settings: SharedSettings; machines: MachineView[] }) {
  const client = useQueryClient();
  const { data: overview, error } = useQuery({ queryKey: factoryKey, queryFn: factoryOverview, retry: false });
  const seen = useRef({ statuses, seq: settings.seq });
  useEffect(() => {
    // Not on mount: the first read is the query's own.
    if (seen.current.statuses === statuses && seen.current.seq === settings.seq) return;
    seen.current = { statuses, seq: settings.seq };
    void client.invalidateQueries({ queryKey: factoryKey });
  }, [client, statuses, settings.seq]);

  const name = (id: string) => machines.find((m) => m.id === id)?.name ?? settings.machines.find((m) => m.id === id)?.name ?? id;

  return (
    <div className="h-full overflow-y-auto px-6 py-5">
      <header className="flex flex-wrap items-center gap-4">
        <h1 className="text-lg font-semibold text-neutral-100">Factory</h1>
        <nav aria-label="Views" className="flex gap-3 text-xs">
          <Link to="/" className="text-sky-300 hover:underline">
            Board
          </Link>
          <Link to="/machines" className="text-sky-300 hover:underline">
            Machines
          </Link>
        </nav>
      </header>
      {error && (
        <p role="alert" className="mt-3 text-sm text-red-400">
          {error.message}
        </p>
      )}
      {overview && (
        <>
          <div className="mt-4">
            <FleetSettings settings={overview.settings} />
          </div>
          {overview.projects.length === 0 && <p className="mt-6 text-neutral-500">No projects yet. Add one from the Projects dialog, then turn its factory on there.</p>}
          {overview.projects.map((entry) => {
            const status = statuses?.[entry.id] ?? entry.status ?? undefined;
            const repo = githubRepo(settings.projects.find((p) => p.id === entry.id)?.repo ?? "");
            return (
              <section key={entry.id} aria-label={entry.name} className="mt-5 rounded-lg border border-neutral-800 p-3">
                <h2 className="font-medium text-neutral-100">{entry.name}</h2>
                {projectLine(entry, status, name).map((line) => (
                  <p key={line} className="text-sm text-neutral-300">
                    {line}
                  </p>
                ))}
                {entry.factory.on && status && <Body status={status} repo={repo} machines={machines} name={name} />}
              </section>
            );
          })}
        </>
      )}
    </div>
  );
}
