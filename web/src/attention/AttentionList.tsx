import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useState } from "react";
import { pollEvery } from "../poll";
import { attentionItems, dismissable, dismissItem, kindLabel, type AttentionItem, type AttentionKind } from "./attention";

/** How often the list is read, so an item shows within 5 seconds of what raised it. */
export const ATTENTION_EVERY = 2000;

const key = ["attention"];

/**
 * What needs a person, across the fleet: a card at the top of the layout's news while anything
 * does, nothing otherwise. Each item says what happened, what to do and on which machine, with its terminal one
 * click away. A "setting overwritten" item is dismissed here; every other clears on its own.
 * `folded` starts it folded to its count.
 */
export function AttentionList({ folded = false }: { folded?: boolean }) {
  const client = useQueryClient();
  const { data: items = [] } = useQuery({
    queryKey: key,
    queryFn: attentionItems,
    refetchInterval: pollEvery(ATTENTION_EVERY, true),
    retry: false,
  });
  const [open, setOpen] = useState(!folded);
  const dismiss = useMutation({
    mutationFn: (item: AttentionItem) => dismissItem(item.id),
    onSettled: () => client.invalidateQueries({ queryKey: key, exact: true }),
  });
  if (items.length === 0) return null;

  return (
    <section
      aria-label="Needs attention"
      className="pointer-events-auto rounded-lg border border-amber-700/60 bg-neutral-900 text-sm shadow-lg shadow-black/50"
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
          {items.map((item) => (
            <li key={item.id} className="space-y-1 px-3 py-2">
              <p className="flex items-baseline gap-2">
                <span className="font-medium text-neutral-100">{kindLabel[item.kind as AttentionKind] ?? item.kind}</span>
                <span className="truncate text-xs text-neutral-500">{item.machine_name}</span>
              </p>
              <p className="text-neutral-200">{item.what}</p>
              <p className="text-neutral-400">{item.todo}</p>
              {(item.terminal || dismissable(item)) && (
                <p className="flex gap-3 pt-1">
                  {item.terminal && (
                    <Link to="/terminals/$terminalId" params={{ terminalId: item.terminal }} className="text-sky-400 hover:text-sky-300">
                      Open terminal
                    </Link>
                  )}
                  {dismissable(item) && (
                    <button
                      type="button"
                      disabled={dismiss.isPending && dismiss.variables?.id === item.id}
                      onClick={() => dismiss.mutate(item)}
                      className="text-sky-400 hover:text-sky-300 disabled:text-neutral-500"
                    >
                      Dismiss
                    </button>
                  )}
                </p>
              )}
              {dismiss.isError && dismiss.variables?.id === item.id && (
                <p role="alert" className="text-red-400">
                  {dismiss.error.message}
                </p>
              )}
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
