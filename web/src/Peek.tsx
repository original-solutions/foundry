import { useQuery } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { machineQuery, sessionTrail } from "./api";
import { useLeaving } from "./Archive";
import { changesQuery } from "./ChangesPanel";
import { totals } from "./changes";
import { branchOf, locate, rows, type MachineView, type WorkspaceView } from "./fleet";
import type { Destination } from "./jump";
import { addsSteps, words, type Glyph } from "./labels";
import { topFiles } from "./lanes";
import { MD, useMediaQuery } from "./media";
import { Now } from "./Now";
import { BOARD_BUSY_EVERY, pollEvery } from "./poll";
import { glyphText, StateGlyph } from "./StateGlyph";
import { StepRow, STEPS_EVERY, useNow } from "./StepsPanel";
import { useRereadOnChange } from "./Timeline";
import { bars, heading } from "./spans";
import { ago, tally, took } from "./trail";

/** How long the highlight rests on a row before the preview reads anything for it. */
export const PEEK_REST = 180;

/** Whether the screen is wide enough for the palette's preview beside its list. */
export function useWide(): boolean {
  return useMediaQuery(MD, false);
}

/** `key`, once it has stayed the same for `ms`; so arrowing through a list reads only where it stops. */
function useRested(key: string | undefined, ms = PEEK_REST): string | undefined {
  const [rested, setRested] = useState<string>();
  useEffect(() => {
    const id = setTimeout(() => setRested(key), ms);
    return () => clearTimeout(id);
  }, [key, ms]);
  return rested;
}

/**
 * The palette's preview of the highlighted row, beside the list on a wide screen: for a terminal,
 * its state, what its session was asked and is doing or said, its last turn's steps, and what its
 * workspace changed; for a new shell, the workspace it starts in. What it reads (a trail, a
 * workspace's changes) is read only for this machine's terminals, once the highlight rests, under
 * the same cache entries as the Steps column and the board.
 */
export function Peek({ at, machines }: { at: Destination | undefined; machines: MachineView[] }) {
  const rested = useRested(at?.key);
  const { data: here } = useQuery(machineQuery);
  if (!at) return <Shell>{null}</Shell>;
  const ready = rested === at.key;
  if (at.kind === "terminal") {
    const found = locate(machines, at.terminal);
    if (!found) return <Shell>{null}</Shell>;
    const mine = found.machine.id === here;
    const row = rows(found.workspace).find((r) => r.terminal === at.terminal)!;
    const session = found.workspace.sessions.find((s) => s.terminal === at.terminal);
    return (
      <Shell label={`Preview of ${at.title}`}>
        <Head glyph={found.glyph} label={found.label} fullLabel={found.fullLabel} name={found.name} workspace={found.workspace} machine={found.machine.name} many={machines.length > 1} />
        {session ? (
          <>
            {session.title && <Said heading="Asked">“{session.title}”</Said>}
            {row.now && (
              <Part heading="Now">
                <Now step={row.now} className="whitespace-normal" />
              </Part>
            )}
            {mine ? <LastTurn session={session.id} glyph={found.glyph} label={found.label} asked={session.title} read={ready} /> : null}
            {row.said && (
              <Part heading="Claude said">
                <p className="line-clamp-8 border-l-2 border-sky-400/40 pl-2 text-xs leading-[1.125rem] whitespace-pre-line text-neutral-300">{row.said.replace(/\n{2,}/g, "\n")}</p>
              </Part>
            )}
          </>
        ) : (
          <p className="text-xs text-neutral-500">A shell has no steps. Open it to see its screen.</p>
        )}
        {mine ? <Changed workspace={found.workspace.id} glyphs={rows(found.workspace).map((r) => r.glyph)} read={ready} /> : <Elsewhere machine={found.machine.name} />}
      </Shell>
    );
  }
  if (at.kind === "shell") {
    const owner = machines.find((m) => m.workspaces.some((w) => w.id === at.workspace));
    const workspace = owner?.workspaces.find((w) => w.id === at.workspace);
    if (!owner || !workspace) return <Shell>{null}</Shell>;
    const all = rows(workspace);
    return (
      <Shell label={`Preview of ${workspace.name}`}>
        <Head workspace={workspace} machine={owner.name} many={machines.length > 1} name={workspace.name} />
        <p className="text-xs text-neutral-400">
          Starts a shell in {workspace.name}, in its worktree on {branchOf(workspace)}.
        </p>
        {all.length > 0 && (
          <Part heading="Its terminals">
            <ul className="space-y-1">
              {all.map((r) => (
                <li key={r.terminal} className="flex items-center gap-2 text-xs">
                  <StateGlyph glyph={r.glyph} />
                  <span className="min-w-0 flex-1 truncate text-neutral-200">
                    {r.name}
                    {r.title && <span className="text-neutral-500"> {r.title}</span>}
                  </span>
                  <span title={r.fullLabel} className={glyphText[r.glyph]}>
                    {r.label}
                  </span>
                </li>
              ))}
            </ul>
          </Part>
        )}
        {owner.id === here ? <Changed workspace={workspace.id} glyphs={all.map((r) => r.glyph)} read={ready} /> : <Elsewhere machine={owner.name} />}
      </Shell>
    );
  }
  if (at.kind === "command") {
    const c = at.command;
    return (
      <Shell label={`Preview of ${c.title}`}>
        <header className="space-y-1.5">
          <p className="text-[11px] font-medium tracking-wide text-neutral-500 uppercase">{c.group}</p>
          <h3 className="flex items-center gap-2 text-base font-semibold text-neutral-50 [&_svg]:size-4">
            {c.icon}
            {c.title}
          </h3>
        </header>
        <p className="text-xs leading-5 text-neutral-400">{c.about}</p>
        {(c.hint || c.keys) && (
          <p className="flex items-center gap-2 text-xs text-neutral-500">
            {c.hint && <span className="text-neutral-300">{c.hint}</span>}
            {c.keys && (
              <span>
                From anywhere: <kbd className="rounded border border-neutral-700 px-1.5 py-0.5 text-[10px] text-neutral-300">{c.keys}</kbd>
              </span>
            )}
          </p>
        )}
      </Shell>
    );
  }
  return (
    <Shell label={`Preview of ${at.title}`}>
      <p className="text-sm text-neutral-200">{at.title}</p>
      <p className="text-xs leading-5 text-neutral-400">Opens the task box with this as Claude's task. Start makes a workspace for it and lands you in its session, already at work.</p>
    </Shell>
  );
}

function Shell({ label, children }: { label?: string; children: React.ReactNode }) {
  return (
    <section
      aria-label={label}
      data-peek
      className="hidden min-h-0 w-[24rem] shrink-0 flex-col gap-3.5 overflow-y-auto border-l border-neutral-800 bg-neutral-950/40 p-4 md:flex"
    >
      {children}
    </section>
  );
}

function Head({
  glyph,
  label,
  fullLabel,
  name,
  workspace,
  machine,
  many,
}: {
  glyph?: Glyph;
  label?: string;
  /** The label uncut, when it cuts a factory run's reason short. */
  fullLabel?: string;
  name: string;
  workspace: WorkspaceView;
  machine: string;
  many: boolean;
}) {
  return (
    <header className="space-y-1">
      {glyph && label && (
        <p className={`flex items-center gap-2 text-xs font-medium ${glyphText[glyph]}`}>
          <StateGlyph glyph={glyph} />
          <span title={fullLabel}>{label}</span>
        </p>
      )}
      <h3 className="text-base font-semibold text-neutral-50">
        {glyph ? (
          <>
            <span className="text-neutral-400">{workspace.name} /</span> {name}
          </>
        ) : (
          name
        )}
      </h3>
      <p className="truncate font-mono text-[11px] text-neutral-500">
        {workspace.project} · {branchOf(workspace)}
        {many && ` · ${machine}`}
      </p>
    </header>
  );
}

function Part({ heading, children }: { heading: string; children: React.ReactNode }) {
  return (
    <div>
      <h4 className="mb-1 text-[11px] font-medium tracking-wide text-neutral-500 uppercase">{heading}</h4>
      {children}
    </div>
  );
}

function Said({ heading, children }: { heading: string; children: React.ReactNode }) {
  return (
    <Part heading={heading}>
      <p className="line-clamp-3 text-sm text-neutral-100">{children}</p>
    </Part>
  );
}

/** The newest turn of a session of this machine: its prompt when it differs from the first, what it did and its newest steps. */
function LastTurn({ session, glyph, label, asked, read }: { session: string; glyph: Glyph; label: string; asked?: string; read: boolean }) {
  const now = useNow();
  const { data } = useQuery({
    // The Steps column's entry, so they share what they read.
    queryKey: ["trail", session],
    queryFn: () => sessionTrail(session),
    enabled: read,
    retry: false,
    refetchInterval: pollEvery(STEPS_EVERY, addsSteps.includes(glyph)),
  });
  useRereadOnChange(`${session}=${label}`);
  const busy = addsSteps.includes(glyph);
  const last = data ? bars(data, busy, now).filter((b) => !b.nudge).at(-1) : undefined;
  if (!last) return null;
  return (
    <Part heading={last.open ? "This turn" : "Last turn"}>
      {last.prompt !== asked && <p className="mb-1 line-clamp-2 text-xs text-neutral-200">{heading(last)}</p>}
      <p className="mb-1 flex gap-2 text-xs text-neutral-500">
        <span>{tally(last.steps)}</span>
        <span className="font-mono">{took(last.end - last.start)}</span>
        <span className="ml-auto">{ago(last.start, now)}</span>
      </p>
      {last.steps.length > 0 && (
        <ol aria-label="Its newest steps" className="ml-1 border-l border-neutral-800 pl-2.5">
          {last.steps.slice(-6).map((step, i) => (
            <StepRow key={i} step={step} now={now} />
          ))}
        </ol>
      )}
    </Part>
  );
}

/** What a workspace of this machine changed since its base, in numbers, its biggest files and its newest commit. */
function Changed({ workspace, glyphs, read }: { workspace: string; glyphs: Glyph[]; read: boolean }) {
  const leaving = useLeaving(workspace);
  const { data, error } = useQuery({
    ...changesQuery(workspace),
    enabled: read && !leaving,
    staleTime: BOARD_BUSY_EVERY,
    retry: false,
    refetchInterval: pollEvery(BOARD_BUSY_EVERY, glyphs.includes("working")),
  });
  let body: React.ReactNode;
  if (leaving) body = <p className="text-xs text-neutral-500">Archiving…</p>;
  else if (error) body = <p className="text-xs text-neutral-500">{words.unreadable}.</p>;
  else if (!data) body = <p className="text-xs text-neutral-600">Reading…</p>;
  else if (data.files.length === 0 && data.commits.length === 0) body = <p className="text-xs text-neutral-500">{words.noChanges}.</p>;
  else {
    const t = totals(data);
    const top = topFiles(data, 4);
    const newest = data.commits.at(0);
    body = (
      <>
        <p className="flex flex-wrap gap-x-2.5 text-xs text-neutral-400">
          <span className="font-mono">
            <span className="text-emerald-400">+{t.added}</span> <span className={t.removed ? "text-red-400" : "text-neutral-600"}>−{t.removed}</span>
          </span>
          <span>
            {t.files} file{t.files === 1 ? "" : "s"}
          </span>
          {data.commits.length > 0 && (
            <span>
              {data.commits.length} commit{data.commits.length === 1 ? "" : "s"}
            </span>
          )}
          {t.uncommitted > 0 && <span className="text-amber-300">{t.uncommitted} uncommitted</span>}
        </p>
        {top.names.length > 0 && (
          <ul className="mt-1 space-y-0.5 font-mono text-[11px] text-neutral-300">
            {top.names.map((name) => (
              <li key={name} className="truncate">
                {name}
              </li>
            ))}
            {top.more > 0 && <li className="text-neutral-500">and {top.more} more</li>}
          </ul>
        )}
        {newest && (
          <p className="mt-1.5 flex gap-2 text-xs">
            <span className="font-mono text-neutral-500">{newest.hash.slice(0, 7)}</span>
            <span className="min-w-0 truncate text-neutral-300">{newest.subject}</span>
          </p>
        )}
      </>
    );
  }
  return <Part heading="Its workspace changed">{body}</Part>;
}

function Elsewhere({ machine }: { machine: string }) {
  return <p className="text-xs text-neutral-500">It runs on {machine}. Its steps and changes are kept there; open it to see its screen.</p>;
}
