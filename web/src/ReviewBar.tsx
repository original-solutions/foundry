import { useBlocker } from "@tanstack/react-router";
import { useState } from "react";
import { plural } from "./labels";
import { button, primaryButton } from "./Modal";
import type { Change, Refusal } from "./reviewDiff";

/**
 * Asks before the page is left with changes not yet saved, whether by a link in the app or by
 * closing the tab. While `dirty` is false nothing is asked.
 */
export function useLeaveGuard(dirty: boolean) {
  useBlocker({
    shouldBlockFn: () => !window.confirm("You have unsaved changes. Leave and lose them?"),
    disabled: !dirty,
    enableBeforeUnload: dirty,
  });
}

/**
 * The bar at the foot of a settings page that collects the changes to shared settings, each as
 * old → new, until they are saved or discarded: "2 changes · Pause claims off → on · Runs at once
 * 3 → 4 · Discard · Save 2 changes". Hidden while nothing changed. Save runs `onSave` and shows
 * each refusal it answers beside its change, or beside Save when it names none. `stale` names the
 * fields that changed elsewhere since they were edited: their rows say so, and Save reads "Look
 * again" and sends nothing until it has been pressed once more. With `onLook`, which lets the page stop marking what was looked at, a field that goes stale again, or another one, asks again; without it the look holds while the same fields are stale.
 */
export function ReviewBar({
  changes,
  onSave,
  onDiscard,
  stale,
  onLook,
}: {
  changes: Change[];
  onSave: () => Promise<Refusal[]>;
  onDiscard: () => void;
  stale: string[];
  /** Told when Look again is pressed, so the page can take the new values as seen and stop marking them. */
  onLook?: () => void;
}) {
  const [refusals, setRefusals] = useState<Refusal[]>([]);
  const [saving, setSaving] = useState(false);
  const [looked, setLooked] = useState("");
  // Refusals belong to the changes they answered: when those change, they go.
  const shape = changes.map((c) => `${c.key}\0${c.before}\0${c.after}`).join("\n");
  const [shown, setShown] = useState(shape);
  if (shape !== shown) {
    setShown(shape);
    setRefusals([]);
  }
  const staleNow = stale.join(",");
  // A look is of the fields stale then: once none is, the next one to go stale needs its own.
  if (staleNow === "" && looked !== "") setLooked("");
  if (changes.length === 0) return null;

  const needsLook = stale.length > 0 && looked !== staleNow;
  const named = new Set(changes.map((c) => c.key));
  const beside = refusals.filter((r) => r.key === undefined || !named.has(r.key));
  const label = `Save ${plural(changes.length, "change")}`;

  const save = async () => {
    if (needsLook) {
      setLooked(staleNow);
      onLook?.();
      return;
    }
    setSaving(true);
    try {
      setRefusals(await onSave());
    } finally {
      setSaving(false);
    }
  };

  return (
    <section aria-label="Unsaved changes" className="sticky bottom-0 z-10 mt-6 flex flex-wrap items-start gap-x-4 gap-y-2 border-t border-neutral-700 bg-neutral-900 px-4 py-3 text-xs text-neutral-200">
      <div className="min-w-0 flex-1">
        <span className="font-medium">{plural(changes.length, "change")}</span>
        <ul className="inline">
          {changes.map((c) => {
            const refused = refusals.filter((r) => r.key === c.key);
            return (
              <li key={c.key} aria-label={c.label} className="inline">
                <span className="text-neutral-600">{" · "}</span>
                <span className={stale.includes(c.key) ? "text-amber-200" : ""}>
                  {c.label} {c.before} → {c.after}
                </span>
                {stale.includes(c.key) && <span className="ml-1.5 text-amber-300"> (changed elsewhere since you edited it)</span>}
                {refused.map((r) => (
                  <span key={r.message} role="alert" className="ml-1.5 text-red-400">
                    {r.message}
                  </span>
                ))}
              </li>
            );
          })}
        </ul>
      </div>
      <div data-beside="save" className="flex flex-wrap items-center justify-end gap-2">
        {beside.map((r) => (
          <p key={r.message} role="alert" className="text-red-400">
            {r.message}
          </p>
        ))}
        <button type="button" onClick={onDiscard} disabled={saving} className={button}>
          Discard
        </button>
        <button type="button" onClick={() => void save()} disabled={saving} className={primaryButton}>
          {needsLook ? "Look again" : label}
        </button>
      </div>
    </section>
  );
}
