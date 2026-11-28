import type { FC } from "react";
import { KeyList } from "./KeySheet";
import { Page, type PageProps } from "./SettingsPage";

/** Settings → Keyboard: the list of keys the sheet shows, read-only. `?` still opens the sheet. */
export const SettingsKeyboard: FC<PageProps> = () => (
  <Page title="Keyboard">
    <p className="text-xs text-neutral-500">Every key Foundry answers, by where it works. Press ? anywhere to see this list in a sheet.</p>
    <div className="mt-4 space-y-4 text-sm">
      <KeyList />
    </div>
  </Page>
);
