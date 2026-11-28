import { useMutation } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useId, useRef, useState, type FC } from "react";
import type { MachineView } from "./fleet";
import { addMachine, movePriority } from "./machineApi";
import { button, field, Modal, primaryButton } from "./Modal";
import { Page, type PageProps } from "./SettingsPage";
import { UpdateChecklist } from "./UpdateChecklist";

/**
 * Settings → Machines: what spans the fleet. The order machines take the coordinator role in,
 * each moved up or down; Add machine; and the checklist that installs the release waiting on the
 * fleet. A machine's own settings are on its page, which each entry of the order links to.
 */
export const SettingsMachines: FC<PageProps> = ({ machines, here }) => (
  <Page title="Machines">
    <div className="flex flex-col gap-8">
      <CoordinatorOrder machines={machines} />
      <div className="flex justify-start">
        <AddMachine machines={machines} />
      </div>
      <UpdateChecklist machines={machines} here={here} />
    </div>
  </Page>
);

/** `order` with the machines at `from` and `to` swapped. */
function swapped(order: string[], from: number, to: number): string[] {
  const next = order.slice();
  [next[from], next[to]] = [next[to], next[from]];
  return next;
}

/**
 * The listed machines in the order they take the coordinator role, the one holding it marked, each
 * moved up or down with the whole new order posted. The list follows the settings that come back.
 */
function CoordinatorOrder({ machines }: { machines: MachineView[] }) {
  const ranked = machines.filter((m) => m.rank !== undefined).sort((a, b) => a.rank! - b.rank!);
  const order = ranked.map((m) => m.id);
  // One move at a time; the list reorders when the new settings arrive.
  // The order the list showed when a move was accepted: the moves wait until the list shows
  // another order, so a second move is never computed from the old one. The wait ends the first
  // time the shown order differs, and is forgotten then, so the old order coming back later
  // (moved elsewhere) does not start it again.
  const [posted, setPosted] = useState<{ from: string }>();
  const shown = order.join(",");
  if (posted !== undefined && posted.from !== shown) setPosted(undefined);
  const move = useMutation({
    mutationFn: async (next: string[]) => {
      await movePriority(next);
      setPosted({ from: shown });
    },
  });
  const moving = move.isPending || posted?.from === shown;
  const heading = useId();
  return (
    <section aria-labelledby={heading}>
      <h3 id={heading} className="text-sm font-semibold text-neutral-100">
        Coordinator order
      </h3>
      <p className="mt-0.5 text-xs text-neutral-500">The first machine online in this order coordinates the fleet.</p>
      {move.error && (
        <p role="alert" className="mt-2 rounded-lg border border-red-400/30 bg-red-400/10 px-3 py-2 text-xs text-red-300">
          {move.error.message}
        </p>
      )}
      <ol aria-label="Coordinator order" className="mt-3 flex flex-col divide-y divide-neutral-800 border-y border-neutral-800">
        {ranked.map((machine, i) => (
          <li key={machine.id} aria-label={machine.name} className="flex items-center gap-3 py-2 text-sm">
            <span className="w-5 text-right text-xs text-neutral-500 tabular-nums">{i + 1}</span>
            <Link to="/settings/machines/$machine" params={{ machine: machine.id }} className="min-w-0 truncate font-medium text-neutral-100 hover:underline">
              {machine.name}
            </Link>
            {machine.acting && <span className="rounded-full bg-sky-400/10 px-2 py-px text-[11px] font-medium text-sky-300">Coordinator</span>}
            {order.length >= 2 && (
              <span className="ml-auto flex gap-1">
                <button
                  type="button"
                  className={button}
                  aria-label={`Move ${machine.name} up in the coordinator order`}
                  disabled={moving || i === 0}
                  onClick={() => move.mutate(swapped(order, i, i - 1))}
                >
                  ↑
                </button>
                <button
                  type="button"
                  className={button}
                  aria-label={`Move ${machine.name} down in the coordinator order`}
                  disabled={moving || i === order.length - 1}
                  onClick={() => move.mutate(swapped(order, i, i + 1))}
                >
                  ↓
                </button>
              </span>
            )}
          </li>
        ))}
      </ol>
    </section>
  );
}

/**
 * The Add machine button and its dialog. The person types the new machine's Tailscale name and
 * gets the one command to run there; a refusal shows in the dialog and keeps the text. Once a
 * listed machine of that name, or at the address it resolved to, arrives in the settings, the
 * dialog says it joined: a machine lists itself under its own Tailscale name, which differs from a
 * full name or an address typed. Closing it forgets the command.
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
