import { useMutationState, useQueries, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useEffect, useId, useRef, useState } from "react";
import { machineQuery, type Changes, type Shipping } from "./api";
import { changesQuery } from "./ChangesPanel";
import { splitPath, totals } from "./changes";
import { byUrgency, rows, type MachineView, type Row, type WorkspaceView } from "./fleet";
import { plural } from "./labels";
import { nextSteps, type NextStep } from "./next";
import { SENT_FOR } from "./NextSteps";
import { BOARD_BUSY_EVERY, pollEvery } from "./poll";
import { reply } from "./Said";
import { stageIndex, stageOf, stages, type Stage } from "./stage";
import { CHECKS_EVERY, ShipLine, shippingQuery } from "./Shipping";
import { glyphText, StateGlyph } from "./StateGlyph";
import { useTerminalActions } from "./TerminalMenu";

interface Placed {
  workspace: WorkspaceView;
  machine: MachineView;
  /** Its terminals, most urgent first. */
  rows: Row[];
  changes: Changes;
  shipping?: Shipping;
  stage: Stage | "none";
}

/** Each stage's colour: work only here is at risk, pushed is safe, in review is on its way, merged is done. */
const tone: Record<Stage, { bar: string; count: string }> = {
  here: { bar: "bg-amber-400/70", count: "text-amber-300" },
  pushed: { bar: "bg-sky-400/70", count: "text-sky-300" },
  review: { bar: "bg-emerald-400/70", count: "text-emerald-400" },
  merged: { bar: "bg-violet-400/70", count: "text-violet-300" },
};

const pending = (s: Shipping | undefined) => s?.pr?.state === "open" && s.pr.checks === "pending";

/**
 * The fleet's work on its way to main, as a scoreboard: every workspace of this machine in a
 * column by how far its work has gone (only here, pushed, in review, merged), each column counted,
 * with the whole fleet's lines and commits summed above. A card shows what the workspace changed,
 * its newest commit and its pull request, and offers an idle session the step that takes the work
 * one column further (commit, push and open a PR, fix failing checks), typed into its terminal. A
 * card that moves forward lights up once in its new column. Workspaces that changed nothing yet are
 * named in a line below; another machine's are named as not read from here, since only this daemon
 * reads its own worktrees.
 *
 * Reads as the board does: each workspace's changes and branch once when the view opens, again
 * when one of its terminals changes state, its changes every `BOARD_BUSY_EVERY` while an agent in
 * it works and its pull request every `CHECKS_EVERY` while its checks run. A workspace being
 * archived is not read.
 */
export function Ship({ machines, onChanges }: { machines: MachineView[]; onChanges: (workspace: string) => void }) {
  const { data: here } = useQuery(machineQuery);
  const leaving = new Set(
    useMutationState({
      filters: { mutationKey: ["archive"], predicate: (m) => m.state.status === "pending" || (m.state.status === "success" && m.state.data === null) },
      select: (m) => String(m.options.mutationKey?.[1]),
    }),
  );
  const all = machines.flatMap((machine) => machine.workspaces.map((workspace) => ({ workspace, machine, rows: byUrgency(rows(workspace)) })));
  const mine = here === undefined ? [] : all.filter((c) => c.machine.id === here && !leaving.has(c.workspace.id));
  const others = here === undefined ? [] : machines.filter((m) => m.id !== here && m.workspaces.length > 0);

  const changes = useQueries({
    queries: mine.map((c) => ({
      ...changesQuery(c.workspace.id),
      staleTime: BOARD_BUSY_EVERY,
      retry: false,
      refetchInterval: pollEvery(BOARD_BUSY_EVERY, c.rows.some((r) => r.glyph === "working")),
    })),
  });
  const shipping = useQueries({
    queries: mine.map((c) => ({
      ...shippingQuery(c.workspace.id),
      staleTime: 30_000,
      retry: false,
      refetchInterval: (query: { state: { data: Shipping | undefined; status: string; error: Error | null } }) => pollEvery(CHECKS_EVERY, pending(query.state.data))(query),
    })),
  });
  useRereadOnStateChange(mine.map((c) => [c.workspace.id, c.rows.map((r) => `${r.terminal}:${r.glyph}`).join(" ")]));

  const placed: Placed[] = [];
  let reading = 0;
  let unreadable = 0;
  mine.forEach((c, i) => {
    const read = changes[i];
    const ship = shipping[i];
    if (read.data && !ship.isPending) placed.push({ ...c, changes: read.data, shipping: ship.data, stage: stageOf(read.data, ship.data) });
    else if (read.isError) unreadable += 1;
    else reading += 1;
  });
  const arrived = useArrivals(placed);

  const working = placed.filter((p) => p.stage !== "none");
  const none = placed.filter((p) => p.stage === "none");
  const sum = working.reduce(
    (acc, p) => {
      const t = totals(p.changes);
      return { added: acc.added + t.added, removed: acc.removed + t.removed, commits: acc.commits + p.changes.commits.length };
    },
    { added: 0, removed: 0, commits: 0 },
  );

  return (
    <section aria-label="Ship" className="flex flex-col gap-4">
      <p className="px-1 text-sm text-neutral-400">
        {working.length === 0 ? (
          reading > 0 ? (
            "Reading where each workspace's work stands…"
          ) : (
            "No workspace has changed anything yet."
          )
        ) : (
          <>
            <span className="font-medium text-neutral-200">{plural(working.length, "workspace")} with work</span>
            {" · "}
            <span className="font-mono">
              <span className="text-emerald-400">+{sum.added.toLocaleString()}</span> <span className={sum.removed ? "text-red-400" : "text-neutral-600"}>−{sum.removed.toLocaleString()}</span>
            </span>
            {" · "}
            {plural(sum.commits, "commit")}
            {reading > 0 && <span className="text-neutral-500"> · reading {reading} more…</span>}
          </>
        )}
      </p>
      <div className="grid gap-3 md:grid-cols-4">
        {stages.map(({ stage, name, hint }, i) => {
          const cards = working.filter((p) => p.stage === stage);
          return (
            <section key={stage} aria-label={name} className="flex min-w-0 flex-col gap-2.5 rounded-xl border border-neutral-800 bg-neutral-900/30 p-2.5">
              <header className="relative flex items-baseline gap-2 px-1.5 pt-1" title={hint}>
                <span aria-hidden="true" className={`absolute -top-2.5 right-1 left-1 h-0.5 rounded-full ${tone[stage].bar}`} />
                <span data-count className={`text-2xl leading-none font-semibold tabular-nums ${cards.length ? tone[stage].count : "text-neutral-600"}`}>
                  {cards.length}
                </span>
                <h3 className="text-sm font-medium text-neutral-200">{name}</h3>
                {i < stages.length - 1 && (
                  <svg viewBox="0 0 16 16" className="ml-auto hidden size-3.5 self-center text-neutral-600 md:block" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                    <path d="M6 3.5 10.5 8 6 12.5" />
                  </svg>
                )}
              </header>
              <p className={`-mt-1.5 px-1.5 text-[11px] text-neutral-500 ${cards.length ? "" : "max-md:hidden"}`}>{hint}</p>
              {cards.map((p) => (
                <ShipCard key={`${p.workspace.id}:${stage}`} placed={p} arrived={arrived[p.workspace.id] === stage} onChanges={onChanges} />
              ))}
              {cards.length === 0 && <p className="rounded-lg border max-md:hidden border-dashed border-neutral-800 px-3 py-4 text-center text-xs text-neutral-600">None</p>}
            </section>
          );
        })}
      </div>
      {none.length > 0 && (
        <section aria-label="Nothing yet" className="flex flex-wrap items-center gap-1.5 px-1 text-xs text-neutral-500">
          <span className="pr-1">Nothing yet:</span>
          {none.map((p) => {
            const lead = p.rows[0];
            const label = (
              <>
                {lead && <StateGlyph glyph={lead.glyph} />}
                {p.workspace.name}
              </>
            );
            return lead ? (
              <Link
                key={p.workspace.id}
                to="/terminals/$terminalId"
                params={{ terminalId: lead.terminal }}
                className="flex items-center gap-1.5 rounded-full border border-neutral-800 px-2 py-0.5 text-neutral-300 hover:border-neutral-600"
              >
                {label}
              </Link>
            ) : (
              <span key={p.workspace.id} className="flex items-center gap-1.5 rounded-full border border-neutral-800 px-2 py-0.5 text-neutral-400">
                {label}
              </span>
            );
          })}
        </section>
      )}
      {unreadable > 0 && <p className="px-1 text-xs text-neutral-500">{plural(unreadable, "workspace")} could not be read; tried again when its terminals change.</p>}
      {others.map((m) => (
        <p key={m.id} className="px-1 text-xs text-neutral-500">
          On {m.name}: {m.workspaces.map((w) => w.name).join(", ")}. Their work is not read from here.
        </p>
      ))}
    </section>
  );
}

/** Marks each workspace's reads out of date when one of its terminals changes state, so the end of a turn shows where it left the work. */
function useRereadOnStateChange(states: [string, string][]) {
  const client = useQueryClient();
  const key = JSON.stringify(states);
  const last = useRef(key);
  useEffect(() => {
    if (last.current === key) return;
    const before = new Map<string, string>(JSON.parse(last.current) as [string, string][]);
    last.current = key;
    for (const [workspace, now] of JSON.parse(key) as [string, string][]) {
      const was = before.get(workspace);
      if (was === undefined || was === now) continue;
      void client.invalidateQueries({ queryKey: changesQuery(workspace).queryKey, exact: true });
      void client.invalidateQueries({ queryKey: shippingQuery(workspace).queryKey, exact: true });
    }
  }, [key, client]);
}

/**
 * The stage each workspace moved forward to since this view first placed it, so its card lights
 * up once on arriving. Kept as state and adjusted while rendering, as React suggests for state
 * that follows props; a step back (a pull request closed) is not marked.
 */
function useArrivals(placed: Placed[]): Record<string, Stage> {
  const now = Object.fromEntries(placed.map((p) => [p.workspace.id, p.stage]));
  const key = JSON.stringify(now);
  const [seen, setSeen] = useState<{ key: string; stages: Record<string, Stage | "none">; arrived: Record<string, Stage> }>({ key, stages: now, arrived: {} });
  if (seen.key !== key) {
    const arrived = { ...seen.arrived };
    for (const [id, stage] of Object.entries(now)) {
      const was = seen.stages[id];
      if (stage === "none" || was === undefined || was === stage) continue;
      if (was === "none" || stageIndex(stage) > stageIndex(was)) arrived[id] = stage;
      else delete arrived[id];
    }
    setSeen({ key, stages: { ...seen.stages, ...now }, arrived });
    return arrived;
  }
  return seen.arrived;
}

function ShipCard({ placed, arrived, onChanges }: { placed: Placed; arrived: boolean; onChanges: (workspace: string) => void }) {
  const id = useId();
  const { workspace, rows, changes, shipping, stage } = placed;
  const terminals = useTerminalActions();
  const lead = rows[0];
  const t = totals(changes);
  const newest = changes.commits.at(0);
  const idle = rows.find((r) => r.session && r.glyph === "ready");
  const steps = idle ? nextSteps(changes, shipping) : [];
  const busy = rows.find((r) => r.session && r.glyph !== "ready" && r.glyph !== "done" && r.glyph !== "away");
  return (
    <article
      aria-labelledby={id}
      data-arrived={arrived || undefined}
      className={`flex min-w-0 flex-col gap-2 rounded-lg border border-neutral-800 bg-neutral-900 px-3 py-2.5 ${arrived ? "motion-safe:animate-arrive" : ""}`}
    >
      <header className="flex min-w-0 items-center gap-2">
        {lead && <StateGlyph glyph={lead.glyph} />}
        <h4 id={id} className="min-w-0 flex-1 truncate text-sm font-semibold text-neutral-100">
          {lead ? (
            <Link to="/terminals/$terminalId" params={{ terminalId: lead.terminal }} className="hover:underline hover:decoration-neutral-600 hover:underline-offset-4">
              {workspace.name}
            </Link>
          ) : (
            workspace.name
          )}
        </h4>
        {busy && <span className={`shrink-0 text-[11px] ${glyphText[busy.glyph]}`}>{busy.label}</span>}
      </header>
      <button
        type="button"
        onClick={() => onChanges(workspace.id)}
        aria-label={`Changes in ${workspace.name}`}
        className="-mx-1 flex min-w-0 flex-col gap-1 rounded px-1 py-0.5 text-left text-xs hover:bg-neutral-800/60"
      >
        <span className="flex flex-wrap items-center gap-x-2 gap-y-0.5 text-neutral-400">
          <span className="font-mono">
            <span className="text-emerald-400">+{t.added}</span> <span className={t.removed ? "text-red-400" : "text-neutral-600"}>−{t.removed}</span>
          </span>
          <span>{plural(t.files, "file")}</span>
          {changes.commits.length > 0 && <span>{plural(changes.commits.length, "commit")}</span>}
          {t.uncommitted > 0 && <span className="text-amber-300">{t.uncommitted} uncommitted</span>}
          {!shipping?.pr && (shipping?.unpushed ?? 0) > 0 && stage === "pushed" && <span className="text-amber-300">{shipping?.unpushed} not pushed</span>}
        </span>
        {newest ? (
          <span className="flex min-w-0 items-baseline gap-1.5">
            <span className="shrink-0 font-mono text-[11px] text-neutral-600">{newest.hash}</span>
            <span className="truncate text-neutral-300">{newest.subject}</span>
          </span>
        ) : (
          <span className="truncate font-mono text-[11px] text-neutral-500">{changes.files.map((f) => splitPath(f.path).name).join(" · ")}</span>
        )}
      </button>
      {shipping?.pr && <ShipLine workspace={workspace.id} name={workspace.name} enabled onArchive={terminals ? () => terminals.archive(workspace.id) : undefined} />}
      {idle && steps.length > 0 && <Steps key={rows.map((r) => r.glyph).join(" ")} steps={steps} to={idle} />}
    </article>
  );
}

/**
 * The steps that take a card's work one column further, each typed into its idle session `to` as
 * a prompt. A pressed step stays down until the session's state changes (the card is keyed by its
 * states, so it starts afresh) or for `SENT_FOR`; one that did not reach it says so.
 */
function Steps({ steps, to }: { steps: NextStep[]; to: Row }) {
  const [sent, setSent] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    if (sent === null) return;
    const timer = setTimeout(() => setSent(null), SENT_FOR);
    return () => clearTimeout(timer);
  }, [sent]);
  const pick = async (step: NextStep) => {
    setSent(step.label);
    setFailed(false);
    try {
      await reply(to.terminal, step.prompt);
    } catch {
      setSent(null);
      setFailed(true);
    }
  };
  return (
    <div role="group" aria-label={`Next for ${to.name}`} className="flex flex-wrap items-center gap-1.5">
      {steps.map((step, i) => (
        <button
            key={step.label}
            type="button"
            title={`Ask ${to.name}: ${step.prompt}`}
            disabled={sent !== null}
            onClick={() => void pick(step)}
            className={`flex h-7 items-center gap-1 rounded-full px-2.5 text-xs font-medium whitespace-nowrap ring-1 transition-colors active:scale-95 disabled:opacity-50 ${
              step.urgent ? "bg-red-400/10 text-red-300 ring-red-400/40 hover:bg-red-400/20" : "bg-emerald-400/10 text-emerald-300 ring-emerald-400/40 hover:bg-emerald-400/20"
            } ${sent === step.label ? "animate-pulse" : ""}`}
          >
            {i === 0 && (
              <svg viewBox="0 0 16 16" className="size-3" fill="currentColor" aria-hidden="true">
                <path d="M8 1.5 9.4 6.6 14.5 8 9.4 9.4 8 14.5 6.6 9.4 1.5 8 6.6 6.6Z" />
              </svg>
            )}
            {step.label}
          </button>
      ))}
      {failed && (
        <span role="alert" className="text-xs text-amber-400">
          Not sent
        </span>
      )}
    </div>
  );
}
