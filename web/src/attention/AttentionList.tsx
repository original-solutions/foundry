import { useMutation, useQuery, useQueryClient, type UseMutationResult } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useId, useState } from "react";
import { pollEvery } from "../poll";
import { attentionItems, byKind, closable, dismissable, firstLine, heading, settle, type AttentionGroup, type AttentionItem } from "./attention";

/** How often the list is read, so an item shows within 5 seconds of what raised it. */
export const ATTENTION_EVERY = 2000;

const key = ["attention"];

/**
 * What needs a person across the fleet that is not a session's question, stall or failure (those
 * lead from the strip beside it, `NextUp`): a card at the head of the page while anything does,
 * nothing otherwise. Each item says what happened, what to do and on which machine, with its terminal one
 * click away. Items of one kind share a card that says their todo once and lists each item on a line of its own. A "setting overwritten" item is dismissed here, and a "setup failed" item clears by closing its
 * exited terminal here; every other clears on its own.
 * `folded` starts it folded to its count, each time the card shows.
 */
export function AttentionList({ folded = false }: { folded?: boolean }) {
  const { data: items = [] } = useQuery({
    queryKey: key,
    queryFn: attentionItems,
    refetchInterval: pollEvery(ATTENTION_EVERY, true),
    retry: false,
  });
  return items.length > 0 ? <Items items={items} folded={folded} /> : null;
}

/** Mounted while anything needs attention, so the fold starts over when the card shows again, as the waiting list's does. */
function Items({ items, folded }: { items: AttentionItem[]; folded: boolean }) {
  const client = useQueryClient();
  const [open, setOpen] = useState(!folded);
  const settling = useMutation({
    mutationFn: settle,
    onSettled: () => client.invalidateQueries({ queryKey: key, exact: true }),
  });

  return (
    <section
      aria-label="Needs attention"
      // Folded, it sits beside the strip to the next that needs you; open, it takes a row of its own.
      className={`${open ? "basis-full" : "flex-[1_1_auto]"} pointer-events-auto rounded-lg border border-amber-700/60 bg-neutral-900 text-sm shadow-lg shadow-black/50`}
    >
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen(!open)}
        className="flex w-full items-center gap-2 px-3 py-2 text-left font-medium text-amber-300 hover:bg-neutral-800/60"
      >
        <span aria-hidden="true">!</span>
        <span className="flex-1">
          {items.length} {items.length === 1 ? "needs" : "need"} attention
        </span>
        <span aria-hidden="true" className="text-neutral-500">
          {open ? "▾" : "▴"}
        </span>
      </button>
      {open && (
        <ul className="max-h-[50vh] divide-y divide-neutral-800 overflow-y-auto border-t border-neutral-800">
          {byKind(items).map((group) =>
            group.items.length === 1 ? (
              <Single key={group.kind} item={group.items[0]} settling={settling} />
            ) : (
              <Group key={group.kind} group={group} settling={settling} />
            ),
          )}
        </ul>
      )}
    </section>
  );
}

type Settling = UseMutationResult<void, Error, AttentionItem>;

/** The only item of its kind: what happened, what to do and its machine, in full. */
function Single({ item, settling }: { item: AttentionItem; settling: Settling }) {
  return (
    <li className="space-y-1 px-3 py-2">
      <p className="flex items-baseline gap-2">
        <span className="font-medium text-neutral-100">{heading(item.kind, 1)}</span>
        <span className="truncate text-xs text-neutral-500">{item.machine_name}</span>
      </p>
      <p className="whitespace-pre-line text-neutral-200">{item.what}</p>
      <p className="text-neutral-400">{item.todo}</p>
      {acts(item) && (
        <p className="flex gap-3 pt-1">
          <Actions item={item} settling={settling} />
        </p>
      )}
      <Failure item={item} settling={settling} />
    </li>
  );
}

/** Several items of one kind: counted once, their shared todo and machine said once, one line each. */
function Group({ group, settling }: { group: AttentionGroup; settling: Settling }) {
  const id = useId();
  return (
    <li aria-labelledby={id} className="space-y-1 px-3 py-2">
      <p className="flex items-baseline gap-2">
        <span id={id} className="font-medium text-neutral-100">
          {heading(group.kind, group.items.length)}
        </span>
        {group.machine_name !== null && <span className="truncate text-xs text-neutral-500">{group.machine_name}</span>}
      </p>
      {group.todo !== null && <p className="text-neutral-400">{group.todo}</p>}
      <ul className="space-y-0.5 pt-1">
        {group.items.map((item) => (
          <Line key={item.id} item={item} group={group} settling={settling} />
        ))}
      </ul>
    </li>
  );
}

/** One item of a group: the first line of what happened, opening to all of it and anything the group does not say for it. */
function Line({ item, group, settling }: { item: AttentionItem; group: AttentionGroup; settling: Settling }) {
  const [open, setOpen] = useState(false);
  return (
    <li>
      <div className="flex items-baseline gap-2">
        {/* w-0: a line is one unwrapped line, and without it the longest one sets the card's width (900 px on a 390 px phone); flex-1 gives it what is left. */}
        <button
          type="button"
          aria-expanded={open}
          title={item.what}
          onClick={() => setOpen(!open)}
          className="flex w-0 min-w-0 flex-1 items-baseline gap-1.5 text-left text-neutral-200 hover:text-neutral-50"
        >
          <span aria-hidden="true" className="text-neutral-500">
            {open ? "▾" : "▸"}
          </span>
          <span className="truncate">{firstLine(item.what)}</span>
        </button>
        {group.machine_name === null && <span className="shrink-0 text-xs text-neutral-500">{item.machine_name}</span>}
        {acts(item) && (
          <span className="flex shrink-0 gap-3">
            <Actions item={item} settling={settling} />
          </span>
        )}
      </div>
      {open && (
        <div className="space-y-1 py-1 pl-4">
          <p className="whitespace-pre-line text-neutral-300">{item.what}</p>
          {group.todo === null && <p className="text-neutral-400">{item.todo}</p>}
        </div>
      )}
      <Failure item={item} settling={settling} />
    </li>
  );
}

/** Whether the item has a link or terminal to open or a button of its own. */
const acts = (item: AttentionItem) => item.issue_url !== null || item.pr_url !== null || item.terminal !== null || dismissable(item) || closable(item);

function Actions({ item, settling }: { item: AttentionItem; settling: Settling }) {
  return (
    <>
      {item.issue_url && (
        <a href={item.issue_url} target="_blank" rel="noreferrer" className="text-sky-400 hover:text-sky-300">
          Issue
        </a>
      )}
      {item.pr_url && (
        <a href={item.pr_url} target="_blank" rel="noreferrer" className="text-sky-400 hover:text-sky-300">
          Pull request
        </a>
      )}
      {item.terminal && (
        <Link to="/terminals/$terminalId" params={{ terminalId: item.terminal }} className="text-sky-400 hover:text-sky-300">
          Open terminal
        </Link>
      )}
      {(dismissable(item) || closable(item)) && (
        <button
          type="button"
          disabled={settling.isPending && settling.variables?.id === item.id}
          onClick={() => settling.mutate(item)}
          className="text-sky-400 hover:text-sky-300 disabled:text-neutral-500"
        >
          {closable(item) ? "Close terminal" : "Dismiss"}
        </button>
      )}
    </>
  );
}

function Failure({ item, settling }: { item: AttentionItem; settling: Settling }) {
  return settling.isError && settling.variables?.id === item.id ? (
    <p role="alert" className="text-red-400">
      {settling.error.message}
    </p>
  ) : null;
}
