import { useLayoutEffect, useRef } from "react";
import { Modal } from "./Modal";

const mod = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent) ? "⌘" : "Ctrl";

/** Every key the app answers, by where it works: the keys to press, and what they do. */
const groups: { where: string; keys: [string[], string][] }[] = [
  {
    where: "Anywhere",
    keys: [
      [[mod, "K"], "Jump to a terminal or workspace, or give Claude a new task"],
      [[mod, "F"], "Find in the open terminal"],
      [[mod, "\\"], "Open a terminal beside this one, or close the split"],
      [[mod, "B"], "Collapse the workspaces list to a rail of states, or expand it"],
      [["Esc"], "Close what is on top: a dialog, sheet, drawer or card"],
      [["?"], "This list"],
    ],
  },
  {
    where: "The task box",
    keys: [
      [[mod, "Enter"], "Start the task"],
      [["Esc"], "Leave the board's empty task line"],
    ],
  },
  {
    where: "The workspaces list",
    keys: [
      [["↑", "↓"], "Move between workspaces and terminals"],
      [["→", "←"], "Unfold or fold a workspace; ← from a terminal goes to its workspace"],
      [["Home", "End"], "The first or the last"],
      [["a–z"], "The next one whose name starts with what you type"],
      [["Enter"], "Open the terminal, or fold the workspace"],
      [["↑", "↓"], "Move along the collapsed rail"],
      [["Home", "End"], "The first or the last on the rail"],
    ],
  },
  {
    where: "The jump palette",
    keys: [
      [["Enter"], "Open"],
      [["Shift", "Enter"], "Open beside the open terminal"],
      [["↑", "↓"], "Move through the list"],
      [["Tab"], "What can be done with the picked terminal"],
      [["Shift", "Tab"], "Back from what can be done to the list"],
      [["⌫"], "Back to the list too, from an empty box"],
    ],
  },
  {
    where: "Find in a terminal",
    keys: [
      [["Enter"], "The older match"],
      [["Shift", "Enter"], "The newer match"],
      [["Esc"], "Close find"],
    ],
  },
  {
    where: "A workspace's changes",
    keys: [
      [["j"], "Next file"],
      [["k"], "Previous file"],
      [["v"], "Mark the file viewed, and go to the next one not viewed"],
      [["[", "]"], "Earlier or later commit; before the first, all changes"],
      [[mod, "Enter"], "Add the comment on a line"],
      [["Esc"], "Drop the comment being written"],
    ],
  },
  {
    where: "A reply box, on a card or a toast",
    keys: [
      [["Enter"], "Send the reply"],
      [["Esc"], "Close the reply box; on a toast, dismiss the toast"],
    ],
  },
];

/** Whether a key typed at `target` is text for a field or a terminal, not a key for the app. */
function typing(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  return target.isContentEditable || target.closest("input, textarea, select, .xterm") !== null;
}

/** Runs `run` on ?, unless it is typed into a field or a terminal. */
export function useHelpKey(run: () => void) {
  const latest = useRef(run);
  useLayoutEffect(() => {
    latest.current = run;
  });
  useLayoutEffect(() => {
    const listener = (event: KeyboardEvent) => {
      if (event.key !== "?" || event.metaKey || event.ctrlKey || event.altKey || typing(event.target)) return;
      event.preventDefault();
      latest.current();
    };
    window.addEventListener("keydown", listener);
    return () => window.removeEventListener("keydown", listener);
  }, []);
}

/** The list of every key the app answers, opened by ? or the sidebar's Keyboard button. */
export function KeySheet({ open, onOpenChange }: { open: boolean; onOpenChange: (open: boolean) => void }) {
  return (
    <Modal open={open} onOpenChange={onOpenChange} wide title="Keyboard" description="Every key Foundry answers, by where it works. Esc closes this.">
      <div className="mt-4 max-h-[70dvh] space-y-4 overflow-y-auto">
        {groups.map((group) => (
          <section key={group.where}>
            <h3 className="mb-1.5 text-[11px] font-semibold tracking-wider text-neutral-500 uppercase">{group.where}</h3>
            <dl className="space-y-1">
              {group.keys.map(([keys, what]) => (
                <div key={what} className="flex items-baseline gap-3">
                  <dt className="flex w-24 shrink-0 gap-1">
                    {keys.map((key) => (
                      <kbd key={key} className="min-w-5 rounded border border-neutral-700 bg-neutral-800 px-1.5 py-px text-center font-sans text-xs text-neutral-200 shadow-[0_1px_0] shadow-neutral-700">
                        {key}
                      </kbd>
                    ))}
                  </dt>
                  <dd className="text-neutral-300">{what}</dd>
                </div>
              ))}
            </dl>
          </section>
        ))}
      </div>
    </Modal>
  );
}
