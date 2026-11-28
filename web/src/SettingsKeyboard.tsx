import type { FC } from "react";
import { keyGroups } from "./KeySheet";
import { Page, type PageProps } from "./SettingsPage";

/** Settings → Keyboard: the list of keys the sheet shows, read-only. `?` still opens the sheet. */
export const SettingsKeyboard: FC<PageProps> = () => (
  <Page title="Keyboard">
    <p className="text-xs text-neutral-500">Every key Foundry answers, by where it works. Press ? anywhere to see this list in a sheet.</p>
    <div className="mt-4 space-y-5">
      {keyGroups.map((group) => (
        <section key={group.where}>
          <h3 className="mb-1.5 text-[11px] font-semibold tracking-wider text-neutral-500 uppercase">{group.where}</h3>
          <dl className="space-y-1.5 text-sm">
            {group.keys.map(([keys, what]) => (
              <div key={what} className="flex items-baseline gap-3">
                <dt className="flex w-28 shrink-0 gap-1">
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
  </Page>
);
