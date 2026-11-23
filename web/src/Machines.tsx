import { useMutation } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useRef, useState, type ReactNode } from "react";
import { post, refused, type Limits } from "./api";
import { byUrgency, rows, type MachineView } from "./fleet";
import { plural, urgency, type Glyph } from "./labels";
import { button, dangerButton, field, fieldLook, Modal, primaryButton } from "./Modal";
import { heardAgo, scaled, span, spanWords, type History, type Point } from "./samples";
import { glyphFill, glyphText, StateGlyph } from "./StateGlyph";
import { useNow } from "./StepsPanel";

const statusText = { online: "text-neutral-400", offline: "text-red-400", unknown: "text-neutral-400" };
const statusDot = { online: "bg-emerald-400", offline: "bg-red-400", unknown: "bg-neutral-500" };

/** A count of terminals in a state, in words. */
const stateWord: Record<Glyph, string> = {
  waiting: "needs you",
  stuck: "stuck",
  failed: "failed",
  working: "working",
  starting: "starting",
  ready: "ready",
  done: "ended",
  away: "unreachable",
};

/** Load per core: grey with room, amber near full, red past one per core. */
const loadTone = (load: number) => (load >= 1 ? "text-red-400" : load >= 0.7 ? "text-amber-400" : "text-neutral-200");
const gb = (bytes: number) => `${(bytes / 1e9).toFixed(1)} GB`;

interface Props {
  machines: MachineView[];
  /** Each machine's samples since this page opened. */
  history: History;
  /** The machine whose daemon serves this page. */
  here?: string;
  connected: boolean;
}

/**
 * Every machine in the fleet as a card: online or offline and when this page last heard from it,
 * its load per core drawn over the last ten minutes (or since the page opened), its free memory and disk, its terminals
 * counted by state and its workspaces, each opening its most urgent terminal. An offline machine
 * keeps its card, greyed, with its last known numbers; nothing of it is hidden. The home's third view,
 * under the head it shares with the board and the timeline.
 */
export function Machines({ machines, history, here, connected }: Props) {
  // Moved on every second, for "heard 4 s ago" and the lines' sliding window.
  const now = useNow(1000);
  return (
    <section aria-label="Machines" className="flex flex-col gap-6">
      {!connected && (
        <p role="status" className="rounded-lg border border-amber-400/30 bg-amber-400/10 px-3 py-2 text-xs text-amber-300">
          Disconnected from the daemon: these are the last numbers this page saw, until it reconnects.
        </p>
      )}
      <div className="flex justify-end">
        <AddMachine machines={machines} />
      </div>
      <div className="grid grid-cols-[repeat(auto-fit,minmax(min(100%,24rem),1fr))] gap-4">
        {machines.map((machine) => (
          <MachineCard key={machine.id} machine={machine} points={history[machine.id]?.points ?? []} heard={history[machine.id]?.heard} here={machine.id === here} known={here !== undefined} now={now} />
        ))}
      </div>
    </section>
  );
}

function MachineCard({ machine, points, heard, here, known, now }: { machine: MachineView; points: Point[]; heard?: number; here: boolean; known: boolean; now: number }) {
  const { sample, status } = machine;
  const away = status === "offline";
  const all = machine.workspaces.flatMap(rows);
  const ago = away ? undefined : heardAgo(heard, now);
  const loads = points.map((p) => p.load);
  const memories = points.map((p) => p.memory);
  // An offline machine's lines stop where it was last heard.
  const end = away ? (points.at(-1)?.at ?? now) : now;
  const covers = span(points, end);
  return (
    <article
      aria-label={machine.name}
      className={`relative flex flex-col overflow-hidden rounded-xl border bg-neutral-900/60 ${away ? "border-red-400/30" : here ? "border-neutral-700" : "border-neutral-800"}`}
    >
      <header className="flex flex-wrap items-center gap-x-2 gap-y-1 px-4 pt-3.5">
        <span aria-hidden="true" className={`size-2 shrink-0 rounded-full ${statusDot[status]}`} />
        <MachineName machine={machine} className="text-[15px] font-semibold">
          {(rename) => (
            <h3 className="min-w-0 truncate text-[15px] font-semibold text-neutral-100" title={rename ? undefined : machine.id}>
              {rename ? (
                <button
                  type="button"
                  onClick={rename}
                  title={`${machine.id}: click to rename`}
                  className="max-w-full truncate rounded text-left hover:underline hover:decoration-neutral-600 hover:underline-offset-4"
                >
                  {machine.name}
                </button>
              ) : (
                machine.name
              )}
            </h3>
          )}
        </MachineName>
        {here && <span className="rounded-full bg-neutral-800 px-2 py-px text-[11px] font-medium text-neutral-300">This machine</span>}
        <span className="ml-auto flex items-baseline gap-1.5 text-xs">
          <span className={statusText[status]}>{status}</span>
          {ago && <span className="text-neutral-500">· heard {ago}</span>}
        </span>
      </header>
      <p className="truncate px-4 font-mono text-[11px] text-neutral-600">{machine.id}</p>
      <div className="flex items-center justify-between gap-2 px-4 pt-1.5">
        <p className="min-w-0 text-xs text-neutral-400">{machine.limits ? limitsText(machine.limits) : "No limits"}</p>
        {machine.listed && <LimitsDialog machine={machine} />}
      </div>
      {machine.listed && known && !here && (
        <div className="flex justify-end px-4 pt-1.5">
          <RemoveDialog machine={machine} />
        </div>
      )}

      {sample ? (
        <div className={away ? "opacity-60 grayscale" : undefined}>
          <div className="flex items-end justify-between gap-3 px-4 pt-4">
            <div
              role="meter"
              aria-label="Load per core"
              aria-valuenow={sample.load_per_core}
              aria-valuemin={0}
              aria-valuemax={Math.max(1, sample.load_per_core)}
              className="flex items-baseline gap-1"
            >
              <span className={`text-3xl font-semibold tabular-nums ${away ? "text-neutral-400" : loadTone(sample.load_per_core)}`}>{sample.load_per_core.toFixed(2)}</span>
              <span className="text-sm text-neutral-500">load per core</span>
            </div>
            <span className="pb-1 text-[11px] text-neutral-600">{spanWords(covers)}</span>
          </div>
          <Spark
            points={points}
            value={(p) => p.load}
            top={Math.max(1.25, ...loads) * 1.08}
            guide={1}
            now={end}
            covers={covers}
            tone={away ? "text-neutral-500" : loadTone(sample.load_per_core)}
            label={loads.length ? `Load per core, ${spanWords(covers)}, from ${Math.min(...loads).toFixed(2)} to ${Math.max(...loads).toFixed(2)}` : "Load per core, no samples yet"}
            tall
          />
          <dl className="mt-3 grid grid-cols-3 border-y border-neutral-800 text-xs">
            <div className="flex flex-col gap-0.5 px-4 py-2.5">
              <dt className="text-neutral-500">Memory free</dt>
              <dd className="font-medium text-neutral-100 tabular-nums">{gb(sample.free_memory)}</dd>
              <Spark
                points={points}
                value={(p) => p.memory}
                top={Math.max(1, ...memories) * 1.15}
                now={end}
                covers={covers}
                tone="text-violet-400"
                label={memories.length ? `Free memory, ${spanWords(covers)}, from ${gb(Math.min(...memories))} to ${gb(Math.max(...memories))}` : "Free memory, no samples yet"}
              />
            </div>
            <div className="flex flex-col gap-0.5 border-l border-neutral-800 px-4 py-2.5">
              <dt className="text-neutral-500">Disk free</dt>
              <dd className="font-medium text-neutral-100 tabular-nums">{gb(sample.free_disk)}</dd>
            </div>
            <div className="flex flex-col gap-0.5 border-l border-neutral-800 px-4 py-2.5">
              <dt className="text-neutral-500">Sessions</dt>
              <dd className="font-medium text-neutral-100 tabular-nums">{sample.sessions}</dd>
            </div>
          </dl>
        </div>
      ) : (
        <p className="mx-4 mt-4 mb-1 rounded-lg border border-dashed border-neutral-800 px-3 py-4 text-center text-xs text-neutral-500">No sample yet</p>
      )}

      {away && (
        <p className="mx-4 mt-3 rounded-lg bg-red-400/10 px-3 py-2 text-xs text-red-300">
          {plural(all.length, "terminal")} unreachable until it is heard from. Its numbers are the last known.
        </p>
      )}

      <States glyphs={all.map((r) => r.glyph)} />

      <ul aria-label={`Workspaces on ${machine.name}`} className="grid grid-cols-[repeat(auto-fill,minmax(16rem,1fr))] gap-x-2 px-2 pt-1 pb-2">
        {machine.workspaces.map((workspace) => {
          const ordered = byUrgency(rows(workspace));
          const lead = ordered[0];
          const inner = (
            <>
              <span className="min-w-0 flex-1 truncate">
                <span className="font-medium">{workspace.name}</span>
                <span className="text-neutral-500"> · {workspace.project}</span>
              </span>
              <span aria-hidden="true" className="flex shrink-0 items-center gap-0.5">
                {ordered.slice(0, 6).map((r) => (
                  <StateGlyph key={r.terminal} glyph={r.glyph} />
                ))}
                {ordered.length > 6 && <span className="pl-0.5 text-[11px] text-neutral-500">+{ordered.length - 6}</span>}
              </span>
            </>
          );
          const row = "flex items-center gap-3 rounded-md px-2 py-1.5 text-sm text-neutral-200";
          return (
            <li key={workspace.id}>
              {lead ? (
                <Link to="/terminals/$terminalId" params={{ terminalId: lead.terminal }} className={`${row} hover:bg-neutral-800/80`}>
                  {inner}
                </Link>
              ) : (
                <span className={row}>{inner}</span>
              )}
            </li>
          );
        })}
        {machine.workspaces.length === 0 && <li className="px-2 py-1.5 text-xs text-neutral-500">No workspaces on this machine</li>}
      </ul>
    </article>
  );
}

/**
 * What an add answered: the command to run on the new machine, how long its code works, and the
 * address the name resolved to.
 */
interface Added {
  name: string;
  command: string;
  expiresInSecs: number;
  address: string;
}

/** Asks the daemon for a join code for the machine Tailscale calls `name`. */
async function addMachine(name: string): Promise<Added> {
  const { status, json } = await post("/api/machines/add", { name });
  if (status !== 201) throw refused(status, json);
  return { name, command: String(json.command), expiresInSecs: Number(json.expires_in_secs), address: String(json.address) };
}

/**
 * The board's Add machine button and its dialog. The person types the new machine's Tailscale
 * name and gets the one command to run there; a refusal shows in the dialog and keeps the text.
 * Once a listed machine of that name, or at the address it resolved to, arrives in the settings,
 * the dialog says it joined: a machine lists itself under its own Tailscale name, which differs
 * from a full name or an address typed. Closing it forgets the command.
 */
function AddMachine({ machines }: { machines: MachineView[] }) {
  const [open, setOpen] = useState(false);
  const [name, setName] = useState("");
  const [copied, setCopied] = useState<"copied" | "selected" | null>(null);
  const command = useRef<HTMLElement>(null);
  const add = useMutation({ mutationFn: addMachine });
  const added = add.data;
  const joined =
    added !== undefined &&
    machines.some((m) => m.listed && (m.name.trim().toLowerCase() === added.name.toLowerCase() || m.address === added.address));
  const change = (next: boolean) => {
    setOpen(next);
    if (!next) {
      setName("");
      setCopied(null);
      add.reset();
    }
  };
  const copy = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied("copied");
    } catch {
      // No clipboard outside a secure context, as on a Tailscale address: select it to copy by hand.
      const range = document.createRange();
      if (command.current) range.selectNodeContents(command.current);
      window.getSelection()?.removeAllRanges();
      window.getSelection()?.addRange(range);
      setCopied("selected");
    }
  };
  return (
    <Modal
      open={open}
      onOpenChange={change}
      trigger="Add machine"
      title="Add machine"
      description={added ? `Run this on ${added.name}, where foundryd is installed.` : "Type the new machine's Tailscale name to get the command to run on it."}
    >
      {added ? (
        <div className="mt-3 flex flex-col gap-2">
          <div className="flex items-start gap-2">
            <pre className="min-w-0 flex-1 overflow-x-auto rounded bg-neutral-950 px-2 py-1.5 text-xs">
              <code ref={command}>{added.command}</code>
            </pre>
            <button type="button" className={button} onClick={() => void copy(added.command)}>
              {copied === "copied" ? "Copied" : "Copy"}
            </button>
          </div>
          <p className="text-xs text-neutral-500">
            {copied === "selected" ? "Selected: press Ctrl+C or ⌘C to copy it. " : ""}
            The code works once and expires in {Math.round(added.expiresInSecs / 60)} minutes.
          </p>
          {joined ? (
            <p role="status" className="text-xs text-emerald-400">
              {added.name} joined
            </p>
          ) : (
            <p className="text-xs text-neutral-500">Waiting for {added.name} to join…</p>
          )}
        </div>
      ) : (
        <form
          className="mt-3 flex flex-col gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            if (name.trim() && !add.isPending) add.mutate(name.trim());
          }}
        >
          <label className="text-xs text-neutral-400">
            Name
            <input
              autoFocus
              autoComplete="off"
              spellCheck={false}
              value={name}
              placeholder="Tailscale name, e.g. trilogy-mac"
              onChange={(e) => setName(e.target.value)}
              className={field}
            />
          </label>
          {add.isError && (
            <p role="alert" className="text-xs text-red-400">
              {add.error.message}
            </p>
          )}
          <div className="flex justify-end">
            <button type="submit" className={primaryButton} disabled={!name.trim() || add.isPending}>
              Add
            </button>
          </div>
        </form>
      )}
    </Modal>
  );
}

/** Bytes as gigabytes for the card's line: at most three decimals. */
const gbShown = (bytes: number) => Number((bytes / 1e9).toFixed(3));

/** A machine's limits in a line. */
const limitsText = (l: Limits) => `At most ${plural(l.max_sessions, "session")} · load ${l.max_load_per_core} per core · ${gbShown(l.min_free_disk)} GB disk free`;

/** What the limits dialog sends: the fields as numbers (a blank one is null, which the daemon refuses), or a clear. */
type LimitsAction = { fields: { max_sessions: number | null; max_load_per_core: number | null; min_free_disk_gb: number | null } } | "clear";

async function sendLimits({ id, action }: { id: string; action: LimitsAction }): Promise<void> {
  const { status, json } = action === "clear" ? await post(`/api/machines/${id}/limits/clear`) : await post(`/api/machines/${id}/limits`, action.fields);
  if (status !== 204) throw refused(status, json);
}

const asNumber = (text: string) => (text.trim() === "" ? null : Number(text));

/**
 * A listed machine's Limits button and its dialog: three number fields filled from its current
 * limits (the disk in gigabytes) or empty, Save to set them and Clear to remove them. The dialog
 * closes once the daemon takes the change; a refusal shows in it and keeps the values. Opening it
 * fills the fields again from the limits then.
 */
function LimitsDialog({ machine }: { machine: MachineView }) {
  const { limits } = machine;
  const [open, setOpen] = useState(false);
  const [sessions, setSessions] = useState("");
  const [load, setLoad] = useState("");
  const [disk, setDisk] = useState("");
  const send = useMutation({ mutationFn: sendLimits, onSuccess: () => setOpen(false) });
  const change = (next: boolean) => {
    if (next) {
      setSessions(limits ? String(limits.max_sessions) : "");
      setLoad(limits ? String(limits.max_load_per_core) : "");
      setDisk(limits ? String(limits.min_free_disk / 1e9) : "");
    }
    send.reset();
    setOpen(next);
  };
  const numberField = (label: string, value: string, set: (text: string) => void, attrs: { min: number | string; step: number | string }) => (
    <label className="text-xs text-neutral-400">
      {label}
      <input type="number" inputMode="decimal" {...attrs} value={value} onChange={(e) => set(e.target.value)} className={field} />
    </label>
  );
  return (
    <Modal open={open} onOpenChange={change} trigger="Limits" title={machine.name} description="Placement skips this machine while it has this many sessions or more, this load per core or more, or less free disk than this.">
      <form
        className="mt-3 flex flex-col gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          if (send.isPending) return;
          send.mutate({ id: machine.id, action: { fields: { max_sessions: asNumber(sessions), max_load_per_core: asNumber(load), min_free_disk_gb: asNumber(disk) } } });
        }}
      >
        {numberField("Max sessions", sessions, setSessions, { min: 1, step: 1 })}
        {numberField("Max load per core", load, setLoad, { min: 0, step: "any" })}
        {numberField("Min free disk (GB)", disk, setDisk, { min: 0, step: "any" })}
        {send.isError && (
          <p role="alert" className="text-xs text-red-400">
            {send.error.message}
          </p>
        )}
        <div className="flex justify-end gap-2">
          <button type="button" className={button} disabled={!limits || send.isPending} onClick={() => send.mutate({ id: machine.id, action: "clear" })}>
            Clear
          </button>
          <button type="submit" className={primaryButton} disabled={send.isPending}>
            Save
          </button>
        </div>
      </form>
    </Modal>
  );
}

/** Removes machine `id` from the fleet: every machine then revokes its key and closes its links. */
async function removeMachine(id: string): Promise<void> {
  const { status, json } = await post(`/api/machines/${encodeURIComponent(id)}/remove`);
  if (status !== 204) throw refused(status, json);
}

/**
 * Another listed machine's Remove from fleet button and its confirmation. Nothing is sent until
 * the person confirms; the dialog closes once the daemon takes it, and a refusal (as when this
 * machine is the one named) shows in it.
 */
function RemoveDialog({ machine }: { machine: MachineView }) {
  const [open, setOpen] = useState(false);
  const remove = useMutation({ mutationFn: removeMachine, onSuccess: () => setOpen(false) });
  const change = (next: boolean) => {
    remove.reset();
    setOpen(next);
  };
  return (
    <Modal
      open={open}
      onOpenChange={change}
      trigger="Remove from fleet"
      title={`Remove ${machine.name}`}
      description={`${machine.name} will be cut off from every machine and cannot join again under its current identity. Its workspaces stay on its disk.`}
    >
      <div className="mt-3 flex flex-col gap-2">
        {remove.isError && (
          <p role="alert" className="text-xs text-red-400">
            {remove.error.message}
          </p>
        )}
        <div className="flex justify-end gap-2">
          <button type="button" className={button} onClick={() => change(false)}>
            Cancel
          </button>
          <button type="button" className={dangerButton} disabled={remove.isPending} onClick={() => remove.mutate(machine.id)}>
            Remove from fleet
          </button>
        </div>
      </div>
    </Modal>
  );
}

/** Renames machine `id` through the daemon, which sends the name to the acting coordinator. */
async function renameMachine({ id, name }: { id: string; name: string }): Promise<void> {
  const { status, json } = await post(`/api/machines/${id}/rename`, { name });
  if (status !== 204) throw refused(status, json);
}

/**
 * A machine's name, renamed in place. `children` draws the name and is given what opens the
 * field, which starts on the name with all of it selected, or nothing for a machine the machine
 * list does not hold yet, which the daemon cannot rename. Enter sends the name, trimmed, and
 * closes the field once the daemon takes it; the new name shows once it arrives in the settings,
 * on every machine. The name it already has sends nothing. Escape closes it and sends nothing.
 * While the name is on its way the field is busy and takes no edits. A refusal shows under the
 * field and keeps the text. `className` sizes the field's text to the
 * name it stands in for.
 */
export function MachineName({
  machine,
  className,
  children,
}: {
  machine: { id: string; name: string; listed: boolean };
  className: string;
  children: (rename: (() => void) | undefined) => ReactNode;
}) {
  const [text, setText] = useState<string | null>(null);
  const rename = useMutation({ mutationFn: renameMachine, onSuccess: () => setText(null) });
  const close = () => {
    setText(null);
    rename.reset();
  };
  if (text === null) return <>{children(machine.listed ? () => setText(machine.name) : undefined)}</>;
  const save = () => {
    const name = text.trim();
    if (name === machine.name) return close();
    if (!rename.isPending) rename.mutate({ id: machine.id, name });
  };
  return (
    <div className="min-w-0 flex-1">
      <input
        aria-label="Machine name"
        autoFocus
        autoComplete="off"
        spellCheck={false}
        value={text}
        readOnly={rename.isPending}
        aria-busy={rename.isPending || undefined}
        onFocus={(e) => e.currentTarget.select()}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            save();
          } else if (e.key === "Escape") {
            e.preventDefault();
            e.stopPropagation();
            close();
          }
        }}
        className={`w-full rounded px-1 py-px aria-busy:opacity-60 ${fieldLook} ${className}`}
      />
      {rename.isError && (
        <p role="alert" className="mt-1 text-xs font-normal text-red-400">
          {rename.error.message}
        </p>
      )}
    </div>
  );
}

/** The machine's terminals counted by state, most urgent first, over a bar of the same shares. */
function States({ glyphs }: { glyphs: Glyph[] }) {
  if (glyphs.length === 0) return null;
  const counts = urgency.map((glyph) => ({ glyph, n: glyphs.filter((g) => g === glyph).length })).filter((c) => c.n > 0);
  return (
    <div className="flex flex-col gap-2 px-4 pt-3 pb-1">
      <div aria-hidden="true" className="flex h-1.5 gap-0.5 overflow-hidden rounded-full">
        {counts.map(({ glyph, n }) => (
          <span key={glyph} className={glyphFill[glyph]} style={{ flexGrow: n }} />
        ))}
      </div>
      <ul aria-label="Terminals by state" className="flex flex-wrap gap-x-3 gap-y-1 text-xs">
        {counts.map(({ glyph, n }) => (
          <li key={glyph} className={`flex items-center gap-1.5 ${glyphText[glyph]}`}>
            <StateGlyph glyph={glyph} still />
            {`${n} ${stateWord[glyph]}`}
          </li>
        ))}
      </ul>
    </div>
  );
}

/**
 * A line of one number over the last `covers` milliseconds, ending at `now` on the right, with the area
 * under it shaded and a dot on the newest point. `guide` draws a dashed line at that value.
 */
function Spark({
  points,
  value,
  top,
  now,
  covers,
  tone,
  label,
  guide,
  tall = false,
}: {
  points: Point[];
  value: (p: Point) => number;
  top: number;
  now: number;
  covers: number;
  tone: string;
  label: string;
  guide?: number;
  tall?: boolean;
}) {
  const h = 30;
  const xy = scaled(points, value, top, now, covers).map(([x, y]) => [x * 100, y * h] as const);
  const line = xy.map(([x, y]) => `${x.toFixed(2)},${y.toFixed(2)}`).join(" ");
  const last = xy.at(-1);
  const guideY = guide === undefined ? undefined : (1 - guide / top) * h;
  return (
    <div className={`relative ${tall ? "mx-4 mt-2 h-16" : "mt-1 h-6"} ${tone}`}>
      <svg role="img" aria-label={label} viewBox={`0 0 100 ${h}`} preserveAspectRatio="none" className="absolute inset-0 size-full overflow-visible">
        {guideY !== undefined && (
          <line x1="0" x2="100" y1={guideY} y2={guideY} className="stroke-neutral-700" strokeDasharray="3 3" vectorEffect="non-scaling-stroke" />
        )}
        {xy.length > 1 && (
          <>
            <polygon points={`${xy[0][0]},${h} ${line} ${last![0]},${h}`} fill="currentColor" opacity={0.12} />
            <polyline points={line} fill="none" stroke="currentColor" strokeWidth={1.5} strokeLinejoin="round" vectorEffect="non-scaling-stroke" />
          </>
        )}
      </svg>
      {guideY !== undefined && (
        <span className="pointer-events-none absolute left-0 -translate-y-full pb-0.5 text-[10px] text-neutral-600" style={{ top: `${(guideY / h) * 100}%` }}>
          1 per core
        </span>
      )}
      {last && (
        <span
          aria-hidden="true"
          className="pointer-events-none absolute size-1.5 -translate-x-1/2 -translate-y-1/2 rounded-full bg-current ring-2 ring-neutral-900"
          style={{ left: `${last[0]}%`, top: `${(last[1] / h) * 100}%` }}
        />
      )}
    </div>
  );
}
