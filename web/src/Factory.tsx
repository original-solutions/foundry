import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { factoryOverview, stopRun, type FactoryOverview, type FactoryStatus, type FactoryWaiting, type FinishedRun, type HeldUp, type LiveRun, type SharedSettings } from "./api";
import { factoryKey, githubRepo, issueUrl, parseRun, projectLine, tally, type Repo, type Tally } from "./factory";
import { Intro, Line, LineNote, SettingsFold, STATIONS } from "./FactoryLine";
import { ProjectFactoryControls } from "./FactorySettings";
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

/** A list under its heading. In a station that says the same thing, the heading is only for a screen reader; `cards` sets each item apart. */
function List({ label, children, quiet = false, cards = false }: { label: string; children: ReactNode; quiet?: boolean; cards?: boolean }) {
  return (
    <div className="mt-2 first:mt-0">
      <h3 className={quiet ? "sr-only" : "text-[0.6875rem] font-medium uppercase tracking-wide text-neutral-500"}>{label}</h3>
      <ul aria-label={label} className={`mt-1 space-y-1.5 text-sm text-neutral-300 ${cards ? "[&>li]:rounded-md [&>li]:border [&>li]:border-neutral-800 [&>li]:bg-neutral-950 [&>li]:px-2 [&>li]:py-1.5" : ""}`}>
        {children}
      </ul>
    </div>
  );
}

function Waiting({ items, label, repo, quiet }: { items: FactoryWaiting[]; label: string; repo?: Repo; quiet?: boolean }) {
  if (items.length === 0) return null;
  return (
    <List label={label} quiet={quiet} cards>
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
    <li>
      <p>
        {parts ? <Issue number={parts.issue} repo={parts} /> : run.run} {run.title}
      </p>
      <p className="mt-0.5 flex flex-wrap gap-x-2 text-xs text-neutral-400">
        <span className="text-sky-300">{run.step}</span>
        <span>{name(run.machine)}</span>
        <span>attempt {parts?.attempt ?? "?"}</span>
        <span>gate retries {run.gate_retries_used}</span>
        <span>{run.minutes_used} min</span>
      </p>
      <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs">
        <WorkspaceLink run={run} machines={machines} />
        {parts && <Stop run={run.run} issue={parts.issue} />}
      </div>
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

/** One station of a project's line as a column, with what stands there or a line saying nothing does. */
function Station({ at, empty, count, children }: { at: keyof Tally; empty: string; count: number; children: ReactNode }) {
  const { label, dot } = STATIONS.find((s) => s.key === at)!;
  const any = count > 0;
  return (
    <div role="group" aria-label={label} className="min-w-0 rounded-md bg-neutral-900/60 p-2">
      <p aria-hidden className="mb-1.5 flex items-center gap-1.5 text-[0.6875rem] font-medium uppercase tracking-wide text-neutral-500">
        <span className={`size-1.5 rounded-full ${any ? dot : "bg-neutral-700"}`} />
        {label}
      </p>
      {any ? children : <p className="text-xs text-neutral-600">{empty}</p>}
    </div>
  );
}

/**
 * What one project's factory is doing, from the status, laid out as the line's stations side by
 * side where there is room (stacked where there is not): what is ready and the feedback rounds that
 * wait, the live runs, the runs held up, and the last finished.
 */
function Body({ status, repo, machines, name }: { status: FactoryStatus; repo?: Repo; machines: MachineView[]; name: (id: string) => string }) {
  const anything = status.ready.length + status.feedback_waiting.length + status.runs.length + status.held_up.length + status.finished.length > 0;
  return (
    <>
      {status.owner_ignored && <p className="mt-1 text-xs text-amber-200">This repo sets factory.owner, which Foundry ignores: the lease decides which machine acts.</p>}
      {anything && (
        <div className="@container mt-3">
          <div className="grid gap-2 @2xl:grid-cols-4">
            <Station at="ready" empty="Nothing ready" count={status.ready.length + status.feedback_waiting.length}>
              <Waiting items={status.ready} label="Ready" repo={repo} quiet />
              <Waiting items={status.feedback_waiting} label="Feedback waiting" repo={repo} />
            </Station>
            <Station at="building" empty="Nothing building" count={status.runs.length}>
              <List label="Live runs" quiet cards>
                {status.runs.map((run) => (
                  <Live key={run.run} run={run} machines={machines} name={name} />
                ))}
              </List>
            </Station>
            <Station at="held" empty="Nothing held up" count={status.held_up.length}>
              <List label="Held up" quiet cards>
                {status.held_up.map((item) => (
                  <Held key={item.run} item={item} />
                ))}
              </List>
            </Station>
            <Station at="finished" empty="Nothing finished yet" count={status.finished.length}>
              <List label="Finished" quiet>
                {status.finished.slice(0, FINISHED_SHOWN).map((item) => (
                  <Done key={item.run} item={item} />
                ))}
              </List>
            </Station>
          </div>
        </div>
      )}
    </>
  );
}

/** The line at the head, the folded settings, and each project, from the overview with the socket's statuses over its own. */
function Overview({
  overview,
  statuses,
  settings,
  machines,
  name,
}: {
  overview: FactoryOverview;
  statuses: Record<string, FactoryStatus> | undefined;
  settings: SharedSettings;
  machines: MachineView[];
  name: (id: string) => string;
}) {
  const statusOf = (entry: FactoryOverview["projects"][number]) => statuses?.[entry.id] ?? entry.status ?? undefined;
  const on = overview.projects.filter((p) => p.factory.on);
  return (
    <>
      <div className="mt-5">
        <Line tally={tally(on.map(statusOf))} label="The line" />
        {on.length > 0 ? <LineNote on={on.length} total={overview.projects.length} paused={overview.settings.paused} /> : <Intro paused={overview.settings.paused} />}
      </div>
      <div className="mt-5">
        <SettingsFold settings={overview.settings} />
      </div>
      {overview.projects.length === 0 && <p className="mt-6 text-neutral-500">No projects yet. Add one from the Projects dialog, then turn its factory on there.</p>}
      {overview.projects.map((entry) => {
        const factory = entry.factory;
        const status = statusOf(entry);
        const project = settings.projects.find((p) => p.id === entry.id);
        const repo = githubRepo(project?.repo ?? "");
        return (
          <section key={entry.id} aria-label={entry.name} className="mt-5 rounded-lg border border-neutral-800 p-3">
            <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-1">
              <h2 className="flex items-center gap-2 font-medium text-neutral-100">
                {entry.name}
                {factory.on && factory.paused && <span className="rounded-full border border-amber-400/40 px-1.5 text-[0.6875rem] font-normal text-amber-200">Claims paused</span>}
              </h2>
              {factory.on && <Line small tally={tally([status])} label={`Line for ${entry.name}`} />}
            </div>
            {project && <ProjectFactoryControls project={project} factory={factory} />}
            {factory.on &&
              projectLine(entry, status, name).map((line) => (
                <p key={line} className="mt-2 text-sm text-neutral-300">
                  {line}
                </p>
              ))}
            {factory.on && status && <Body status={status} repo={repo} machines={machines} name={name} />}
          </section>
        );
      })}
    </>
  );
}

/**
 * The Factory view, headed by the line: how many issues stand ready, building, held up and finished
 * across every project that is on (or, with none on, what the factory does and what it needs), and
 * the fleet's settings folded to one line that opens to their form. Then each project: its own
 * line, its switch, login and pause, what its lease holder says (or why it is idle), the ready
 * queue with each issue's reason, feedback rounds that wait, live runs each with a link to its
 * workspace and a Stop that asks first, runs held up, and the last 20 finished. The statuses the events socket sends win over what the
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
      {overview && <Overview overview={overview} statuses={statuses} settings={settings} machines={machines} name={name} />}
    </div>
  );
}
